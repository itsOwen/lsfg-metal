// private drawable handed to the game instead of the layer's real drawables
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use block2::RcBlock;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, Message};
use objc2_foundation::NSObject;
use objc2_metal::{
    MTL4CommandQueue, MTLDrawable, MTLDrawablePresentedHandler, MTLEvent, MTLTexture,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

use super::generator::{Generator, Job};
use super::hooks;

pub type Handler = RcBlock<dyn Fn(NonNull<ProtocolObject<dyn MTLDrawable>>)>;

#[derive(Default)]
pub struct State {
    pub presented_time: f64,
    // non-zero once a metal 4 queue signalled the game event for this frame
    pub serial: u64,
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

    impl ProxyDrawable {
        // metal 4 drops presentDrawable: and commit, so the queue synchronises with the drawable

        // nothing to wait for: the pool only hands out a texture the worker has finished with
        #[unsafe(method(waitOnCommandQueue:))]
        fn __wait_on_command_queue(&self, _queue: *mut AnyObject) {}

        // the game's work on our texture ends here, where commit signalled on the legacy path
        #[unsafe(method(signalOnCommandQueue:))]
        fn __signal_on_command_queue(&self, queue: *mut AnyObject) {
            let Some(gen) = self.ivars().owner else {
                return;
            };
            if queue.is_null() {
                return;
            }
            // a queue that cannot signal events leaves the serial at zero and present falls back
            let responds: bool =
                unsafe { msg_send![queue, respondsToSelector: sel!(signalEvent:value:)] };
            if !responds {
                return;
            }
            let queue = unsafe { &*(queue as *const ProtocolObject<dyn MTL4CommandQueue>) };
            let serial = gen.next_serial();
            queue.signalEvent_value(
                ProtocolObject::<dyn MTLEvent>::from_ref(gen.game_event()),
                serial,
            );
            self.ivars().state.lock().unwrap().serial = serial;
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

    // direct present on our drawable: wait on the serial metal 4 signalled, else the last commit
    fn submit(&self, duration: f64) {
        let Some(gen) = self.ivars().owner else {
            return;
        };
        let serial = self.ivars().state.lock().unwrap().serial;
        let cb = if serial == 0 {
            hooks::last_committed()
        } else {
            None
        };
        gen.enqueue(Job {
            latency: Default::default(),
            cb,
            drawable: self.retain(),
            duration,
            serial,
            sample: gen.frame_sample(),
        });
    }
}
