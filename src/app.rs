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

        // --- M2 --------------------------------------------------------------------------------
        // Runs in every process: browser, renderer, GPU, zygote. Keep it pure — there is no state
        // to reach for out here, and a renderer that missed this call refuses to load bru:// at all.
        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            crate::chrome::register_scheme(registrar);
        }

        // --- M4 --------------------------------------------------------------------------------
        // Renderer process only. It must not touch browser-process state — same binary, different
        // process — and everything it needs is the renderer half of the message router.
        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(BruRenderProcessHandler::new())
        }
    }
}

// --- M4 ----------------------------------------------------------------------------------------
// The renderer side of the message router: three forwards, exactly as its trait documents. The
// router is a per-process singleton inside ipc.rs, because CEF may ask for this handler more than
// once and two routers in one renderer would each answer half the queries.
wrap_render_process_handler! {
    struct BruRenderProcessHandler;

    impl RenderProcessHandler {
        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            crate::ipc::renderer_on_context_created(browser, frame, context);
        }

        fn on_context_released(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            crate::ipc::renderer_on_context_released(browser, frame, context);
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            crate::ipc::renderer_on_process_message_received(
                browser,
                frame,
                source_process,
                message,
            )
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

            // M2: the factory has to exist before anything can ask for a bru:// URL.
            crate::chrome::register_factory();

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
