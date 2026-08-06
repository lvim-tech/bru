//! The application object, and the one callback that matters at startup.

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::keys::BruClient;
use crate::state::{schedule_close, BruState};
use crate::window::{BruBrowserViewDelegate, BruWindowDelegate};

/// Where bru goes with no argument.
const HOME: &str = "https://start.duckduckgo.com/";

wrap_app! {
    pub struct BruApp {
        state: Arc<Mutex<BruState>>,
    }

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(BruBrowserProcessHandler::new(self.state.clone()))
        }
    }
}

wrap_browser_process_handler! {
    struct BruBrowserProcessHandler {
        state: Arc<Mutex<BruState>>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let command_line = command_line_get_global().expect("no global command line");

            // CEF asks for the client again through default_client when it creates popups, and
            // handing out a fresh one each time loses the handlers. It goes in the shared state
            // rather than in this handler because CEF builds a new handler object per callback.
            self.state
                .lock()
                .expect("state mutex poisoned")
                .set_client(BruClient::new(self.state.clone()));

            let url = CefString::from(&command_line.switch_value(Some(&CefString::from("url"))))
                .to_string();
            let url = CefString::from(if url.is_empty() { HOME } else { url.as_str() });

            let settings = BrowserSettings::default();
            let mut client = self.default_client();
            let mut view_delegate = BruBrowserViewDelegate::new();

            let browser_view = browser_view_create(
                client.as_mut(),
                Some(&url),
                Some(&settings),
                None,
                None,
                Some(&mut view_delegate),
            );

            let mut window_delegate =
                BruWindowDelegate::new(self.state.clone(), RefCell::new(browser_view));
            window_create_top_level(Some(&mut window_delegate));

            // Debug hook, off unless asked for. See state::schedule_close.
            let close_after =
                CefString::from(&command_line.switch_value(Some(&CefString::from("close-after-ms"))))
                    .to_string();
            if let Ok(delay_ms) = close_after.parse::<i64>() {
                schedule_close(delay_ms);
            }
        }

        fn default_client(&self) -> Option<Client> {
            self.state.lock().expect("state mutex poisoned").client()
        }
    }
}
