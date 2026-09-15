// recursive mutex for the api mutex and the per-device queue mutex; unlike std::sync::ReentrantLock, new() is const so it can build a static
use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};

pub struct Recursive {
    state: Mutex<(Option<ThreadId>, u32)>,
    cv: Condvar,
}

pub struct Guard<'a>(&'a Recursive);

impl Recursive {
    pub const fn new() -> Self {
        Recursive {
            state: Mutex::new((None, 0)),
            cv: Condvar::new(),
        }
    }

    pub fn lock(&self) -> Guard<'_> {
        let me = thread::current().id();
        let mut s = self.state.lock().unwrap();
        while s.0.is_some_and(|owner| owner != me) {
            s = self.cv.wait(s).unwrap();
        }
        *s = (Some(me), s.1 + 1);
        Guard(self)
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let mut s = self.0.state.lock().unwrap();
        s.1 -= 1;
        if s.1 == 0 {
            s.0 = None;
            self.0.cv.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reentrant_and_exclusive() {
        static M: Recursive = Recursive::new();
        let a = M.lock();
        let b = M.lock();
        let t = std::thread::spawn(|| {
            let _c = M.lock();
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!t.is_finished());
        drop(a);
        drop(b);
        t.join().unwrap();
    }
}
