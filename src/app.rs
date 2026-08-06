//! The application object, and the one callback that matters at startup.

use cef::*;
use std::cell::RefCell;

use crate::keys::BruClient;
use crate::window::{BruBrowserViewDelegate, BruWindowDelegate};

/// Where bru goes with no argument.
const HOME: &str = "https://start.duckduckgo.com/";

wrap_app! {
    pub struct BruApp;

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(BruBrowserProcessHandler::new(RefCell::new(None)))
        }
    }
}

wrap_browser_process_handler! {
    struct BruBrowserProcessHandler {
        client: RefCell<Option<Client>>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let command_line = command_line_get_global().expect("no global command line");

            // The client is kept here because CEF asks for it again through default_client when it
            // creates popups, and handing out a fresh one each time loses the handlers.
            *self.client.borrow_mut() = Some(BruClient::new());

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

            let mut window_delegate = BruWindowDelegate::new(RefCell::new(browser_view));
            window_create_top_level(Some(&mut window_delegate));
        }

        fn default_client(&self) -> Option<Client> {
            self.client.borrow().clone()
        }
    }
}
