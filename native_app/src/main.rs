#![allow(clippy::type_complexity)]

#[macro_use]
extern crate common;

extern crate simulation;

#[allow(unused_imports)]
#[macro_use]
extern crate inline_tweak;

#[macro_use]
mod uiworld;

mod audio;
mod automation;
mod debug_gui;
mod frame_profile;
mod game_loop;
mod gui;
mod init;
mod inputmap;
mod network;
mod rendering;
mod service_icon;
mod session;
mod sim_thread;

fn main() {
    #[cfg(feature = "profile")]
    profiling::tracy_client::Client::start();
    profiling::register_thread!("Main Thread");

    if let Err(error) = automation::configure() {
        automation::fail(&error);
    }
    engine::framework::init();
    init::init();

    #[cfg(windows)]
    let _timer_resolution = windows_timer::TimerResolution::request();
    engine::framework::start::<game_loop::State>();
}

#[cfg(windows)]
mod windows_timer {
    const PERIOD_MS: u32 = 1;

    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
        fn timeEndPeriod(period: u32) -> u32;
    }

    pub(super) struct TimerResolution;

    impl TimerResolution {
        pub(super) fn request() -> Option<Self> {
            // SAFETY: WinMM accepts a period in milliseconds and retains no pointers.
            let result = unsafe { timeBeginPeriod(PERIOD_MS) };
            if result == 0 {
                Some(Self)
            } else {
                log::warn!("Could not request a 1 ms Windows timer period ({result})");
                None
            }
        }
    }

    impl Drop for TimerResolution {
        fn drop(&mut self) {
            // SAFETY: This guard exists only after a successful request for the same period.
            unsafe { timeEndPeriod(PERIOD_MS) };
        }
    }
}
