//! The hidden webview that drives `reel peek` and `reel render`.

use gpui_kit::*;
use gpui_webview::WebView;

pub struct CaptureView {
    pub webview: Entity<WebView>,
    pub current_job: Option<u64>,
}

impl CaptureView {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let builder = wry::WebViewBuilder::new().with_url("about:blank");
        let webview =
            cx.new(|cx| WebView::build(builder, window, cx).expect("creating capture webview"));
        Self {
            webview,
            current_job: None,
        }
    }

    pub fn load(&mut self, job: u64, url: &str, cx: &mut Context<Self>) {
        self.current_job = Some(job);
        log::info!("capture job {job}: {url}");
        self.webview.update(cx, |webview, _| webview.load_url(url));
        cx.notify();
    }
}

impl Render for CaptureView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Kept visible at 1x1 so WebKit does not throttle video decoding while rendering.
        div().absolute().size(px(1.)).child(self.webview.clone())
    }
}
