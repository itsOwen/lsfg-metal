// private drawable handed to the game instead of the layer's real drawables
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use block2::RcBlock;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, Message};
use objc2_foundation::NSObject;
use objc2_metal::{MTLDrawable, MTLDrawablePresentedHandler, MTLTexture};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use super::generator::{Generator, Job};
use super::hooks;

pub type Handler = RcBlock<dyn Fn(NonNull<ProtocolObject<dyn MTLDrawable>>)>;

#[derive(Default)]
pub struct State {
    pub presented_time: f64,
    pub handlers: Vec<Handler>,
    pub release: Option<Box<dyn FnOnce() + Send>>,
}

pub struct Ivars {
    pub texture: Retained<ProtocolObject<dyn MTLTexture>>,
    pub layer: Weak<CAMetalLayer>,
    pub id: usize,
    pub state: Mutex<State>,
    pub owner: Option<&'static Generator>,
    // the texture already went back to the pool
    pub recycled: AtomicBool,
}

impl Drop for Ivars {
    fn drop(&mut self) {
        if let Some(r) = self.state.get_mut().unwrap().release.take() {
            r();
        }
        if !*self.recycled.get_mut() {
            if let Some(g) = self.owner {
                g.pool_return(self.texture.clone());
            }
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "LSFGMProxyDrawable"]
    #[ivars = Ivars]
    pub struct ProxyDrawable;

    unsafe impl NSObjectProtocol for ProxyDrawable {}

    unsafe impl MTLDrawable for ProxyDrawable {
        #[unsafe(method(present))]
        fn __present(&self) {
            self.submit(0.0)
        }

        // the requested time is dropped; pacing decides when the frame is shown
        #[unsafe(method(presentAtTime:))]
        fn __present_at_time(&self, _time: f64) {
            self.submit(0.0)
        }

        #[unsafe(method(presentAfterMinimumDuration:))]
        fn __present_after(&self, duration: f64) {
            self.submit(duration)
        }

        #[unsafe(method(addPresentedHandler:))]
        fn __add_presented_handler(&self, block: MTLDrawablePresentedHandler) {
            if let Some(b) = unsafe { RcBlock::copy(block) } {
                self.ivars().state.lock().unwrap().handlers.push(b);
            }
        }

        #[unsafe(method(presentedTime))]
        fn __presented_time(&self) -> f64 {
            self.ivars().state.lock().unwrap().presented_time
        }

        #[unsafe(method(drawableID))]
        fn __drawable_id(&self) -> usize {
            self.ivars().id
        }
    }

    unsafe impl CAMetalDrawable for ProxyDrawable {
        #[unsafe(method_id(texture))]
        fn __texture(&self) -> Retained<ProtocolObject<dyn MTLTexture>> {
            self.ivars().texture.clone()
        }

        #[unsafe(method_id(layer))]
        fn __layer(&self) -> Option<Retained<CAMetalLayer>> {
            self.ivars().layer.load()
        }
    }
);

impl ProxyDrawable {
    pub fn new(
        texture: Retained<ProtocolObject<dyn MTLTexture>>,
        layer: &Retained<CAMetalLayer>,
        id: usize,
        owner: Option<&'static Generator>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars {
            texture,
            layer: Weak::from_retained(layer),
            id,
            state: Mutex::default(),
            owner,
            recycled: AtomicBool::new(false),
        });
        unsafe { msg_send![super(this), init] }
    }

    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.ivars().texture
    }

    pub fn owner(&self) -> Option<&'static Generator> {
        self.ivars().owner
    }

    // returns the texture even when the game still holds the drawable, as the layer's own do
    pub fn recycle(&self) {
        let iv = self.ivars();
        if !iv.recycled.swap(true, Ordering::AcqRel) {
            if let Some(g) = iv.owner {
                g.pool_return(iv.texture.clone());
            }
        }
    }

    pub fn set_release(&self, f: impl FnOnce() + Send + 'static) {
        self.ivars().state.lock().unwrap().release = Some(Box::new(f));
    }

    // the real drawable that shows this frame reports back
    pub fn presented(&self, time: f64) {
        let handlers = {
            let mut s = self.ivars().state.lock().unwrap();
            s.presented_time = time;
            std::mem::take(&mut s.handlers)
        };
        let me = NonNull::from(ProtocolObject::<dyn MTLDrawable>::from_ref(self));
        for h in handlers {
            h.call((me,));
        }
    }

    // direct present on our drawable: pair it with the last command buffer the game committed
    fn submit(&self, duration: f64) {
        let Some(gen) = self.ivars().owner else {
            return;
        };
        let cb = hooks::last_committed();
        gen.enqueue(Job {
            latency: Default::default(),
            cb,
            drawable: self.retain(),
            duration,
            serial: 0,
            sample: gen.frame_sample(),
        });
    }
}
