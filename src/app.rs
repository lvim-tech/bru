//! The application object, and the one callback that matters at startup.

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::keys::BruClient;
use crate::state::{schedule_close, BruState};
use crate::window::{BruBrowserViewDelegate, BruChromeViewDelegate, BruWindowDelegate};

/// Where bru goes with no argument.
const HOME: &str = "https://start.duckduckgo.com/";

// MERGE NOTE (M2): these two data: URIs are placeholders standing in for the bru:// scheme, which
// M2 implements. When it lands they become "bru://chrome/top.html" and "bru://chrome/bottom.html"
// and the two constants below go away with them.
const TOP_PLACEHOLDER: &str = r#"<!doctype html><meta charset="utf-8"><body style="margin:0;height:100vh;display:flex;align-items:center;background:#1a6fb5;color:#fff;font:13px/1 monospace"><span style="padding:0 8px">top</span></body>"#;
const BOTTOM_PLACEHOLDER: &str = r#"<!doctype html><meta charset="utf-8"><body style="margin:0;height:100vh;display:flex;align-items:center;background:#b5651a;color:#fff;font:13px/1 monospace"><span style="padding:0 8px">bottom</span></body>"#;

/// Chrome strip heights, in logical pixels.
const TOP_HEIGHT: i32 = 28;
const BOTTOM_HEIGHT: i32 = 24;

/// A page CEF can load without a scheme handler behind it.
fn data_uri(content: &str, mime_type: &str) -> CefString {
    let encoded = CefString::from(&base64_encode(Some(content.as_bytes())));
    let escaped = CefString::from(&uriencode(Some(&encoded), 0)).to_string();
    CefString::from(format!("data:{mime_type};base64,{escaped}").as_str())
}

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

            // All three share one Client, so one set of handlers serves the page and the chrome.
            // Which browser an event came from is answered by its identifier, not by its handler.
            let mut top_delegate =
                BruChromeViewDelegate::new(self.state.clone(), TOP_HEIGHT);
            let top_view = browser_view_create(
                client.as_mut(),
                Some(&data_uri(TOP_PLACEHOLDER, "text/html")),
                Some(&settings),
                None,
                None,
                Some(&mut top_delegate),
            );

            let mut page_delegate = BruBrowserViewDelegate::new();
            let page_view = browser_view_create(
                client.as_mut(),
                Some(&url),
                Some(&settings),
                None,
                None,
                Some(&mut page_delegate),
            );

            let mut bottom_delegate =
                BruChromeViewDelegate::new(self.state.clone(), BOTTOM_HEIGHT);
            let bottom_view = browser_view_create(
                client.as_mut(),
                Some(&data_uri(BOTTOM_PLACEHOLDER, "text/html")),
                Some(&settings),
                None,
                None,
                Some(&mut bottom_delegate),
            );

            let mut window_delegate = BruWindowDelegate::new(
                self.state.clone(),
                RefCell::new(top_view),
                RefCell::new(page_view),
                RefCell::new(bottom_view),
            );
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
