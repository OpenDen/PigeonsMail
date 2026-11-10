//! HTML email rendering into an RGBA viewport using `litehtml`, plus link hit-testing.
//!
//! Remote content is never fetched here: remote image URLs are only *reported*, and the caller
//! decides whether to download them and hand the bytes back via [`EmailView::load_remote_images`].
use std::collections::HashSet;

use anyhow::{anyhow, Result};
use litehtml::email::{prepare_email_html, EMAIL_MASTER_CSS};
use litehtml::pixbuf::PixbufContainer;
use litehtml::selection::Selection;
use litehtml::{Document, DocumentContainer, DrawContext, Position};

use crate::mime::{text_to_html, ParsedEmail};

pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Opaque RGBA (row-major), composited over white.
    pub rgba: Vec<u8>,
}

/// A laid-out email. Dropping it frees the parsed document, its images and the pixel buffer.
///
/// Not `Send`: it must stay on the thread that created it.
pub struct EmailView {
    doc: Option<Document<'static>>,
    /// Owned allocation backing `doc`'s container borrow; freed after `doc` in `Drop`.
    container: *mut PixbufContainer,
    width: u32,
    height: u32,
    scale: f32,
    content_height: f32,
    remote_images: Vec<String>,
    loaded_remote: HashSet<String>,
    /// Document-coordinate selection; its element pointers are only valid for `doc`, so it is
    /// cleared on every relayout.
    selection: Selection<'static>,
}

impl EmailView {
    /// `width`/`height` are the logical viewport size; `scale` is the display scale factor.
    pub fn new(email: &ParsedEmail, width: u32, height: u32, scale: f32) -> Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        let scale = if scale.is_finite() && scale >= 1.0 { scale.min(4.0) } else { 1.0 };

        let empty = text_to_html("");
        let source = email.html.as_deref().unwrap_or(&empty);
        // The body is already UTF-8; the BOM stops `<meta charset>` from triggering a re-decode.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(source.as_bytes());

        let cid = |cid: &str| -> Option<Vec<u8>> {
            email.cid_images.iter().find(|(id, _)| id == cid).map(|(_, b)| b.clone())
        };
        let prepared = prepare_email_html(&bytes, Some(&cid), None);

        let mut container = Box::new(PixbufContainer::new_with_scale(width, height, scale));
        for (uri, data) in &prepared.images {
            container.load_image_data(uri, data);
        }
        let ptr = Box::into_raw(container);

        // SAFETY: `ptr` stays allocated until `Drop`, which drops the document first. While the
        // document exists the container is only touched through `Document::with_container_mut`.
        let static_ref: &'static mut PixbufContainer = unsafe { &mut *ptr };
        let doc = match Document::from_html(&prepared.html, static_ref, Some(EMAIL_MASTER_CSS), None) {
            Ok(d) => d,
            Err(e) => {
                // SAFETY: the failed constructor kept no reference to the container.
                unsafe { drop(Box::from_raw(ptr)) };
                return Err(anyhow!("cannot parse email HTML: {e}"));
            }
        };

        let mut view = Self {
            doc: Some(doc),
            container: ptr,
            width,
            height,
            scale,
            content_height: 0.0,
            remote_images: Vec::new(),
            loaded_remote: HashSet::new(),
            selection: Selection::new(),
        };
        view.relayout();

        let pending = view.with_container(|c| c.take_pending_images());
        view.remote_images = pending
            .into_iter()
            .map(|(url, _)| url)
            .filter(|u| is_remote_url(u))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        view.remote_images.sort();
        Ok(view)
    }

    fn with_container<R>(&mut self, f: impl FnOnce(&mut PixbufContainer) -> R) -> R {
        let doc = self.doc.as_mut().expect("document is present until drop");
        // SAFETY: the container was created as a `PixbufContainer`, and no litehtml call is
        // running while the closure executes.
        unsafe {
            doc.with_container_mut(|c| {
                let p = c as *mut dyn DocumentContainer as *mut PixbufContainer;
                f(&mut *p)
            })
        }
    }

    fn relayout(&mut self) {
        self.selection.clear();
        let width = self.width as f32;
        let doc = self.doc.as_mut().expect("document is present until drop");
        let _ = doc.render(width);
        self.content_height = doc.height();
    }

    pub fn content_height(&self) -> u32 {
        self.content_height.ceil() as u32
    }

    /// Resize without reparsing the message or discarding decoded images.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        self.with_container(|c| c.resize(width, height));
        let doc = self.doc.as_mut().expect("document is present until drop");
        doc.media_changed();
        self.relayout();
    }

    /// Remote image URLs referenced by the message (not loaded unless provided via
    /// [`load_remote_images`](Self::load_remote_images)).
    pub fn remote_images(&self) -> &[String] {
        &self.remote_images
    }

    pub fn blocked_remote_images(&self) -> usize {
        self.remote_images.iter().filter(|u| !self.loaded_remote.contains(*u)).count()
    }

    pub fn unloaded_remote_images(&self) -> impl Iterator<Item = &String> {
        self.remote_images.iter().filter(|u| !self.loaded_remote.contains(*u))
    }

    /// Supplies downloaded remote images and re-lays out the document.
    pub fn load_remote_images(&mut self, images: Vec<(String, Vec<u8>)>) {
        if images.is_empty() {
            return;
        }
        for (url, _) in &images {
            self.loaded_remote.insert(url.clone());
        }
        self.with_container(|c| {
            for (url, data) in &images {
                c.load_image_data(url, data);
            }
        });
        self.relayout();
    }

    /// Renders the part of the document starting `scroll_y` pixels from the top.
    pub fn render_viewport(&mut self, scroll_y: u32) -> Frame {
        self.render_viewport_with(scroll_y, |width, height, premul| Frame {
            width,
            height,
            rgba: flatten_on_white(premul),
        })
    }

    /// Draw a viewport and expose its premultiplied RGBA pixels to the callback.
    pub fn render_viewport_with<R>(
        &mut self,
        scroll_y: u32,
        f: impl FnOnce(u32, u32, &[u8]) -> R,
    ) -> R {
        let (w, h, scale) = (self.width, self.height, self.scale);
        let max_scroll = self.content_height().saturating_sub(h);
        let scroll = scroll_y.min(max_scroll) as f32;

        // Selection rectangles are in document coordinates; move them into the viewport.
        let rects: Vec<Position> = self
            .selection
            .rectangles()
            .iter()
            .map(|r| Position { x: r.x, y: r.y - scroll, width: r.width, height: r.height })
            .collect();

        // Resizing clears the pixmap, so frames never accumulate on each other.
        self.with_container(|c| c.resize_with_scale(w, h, scale));
        let doc = self.doc.as_mut().expect("document is present until drop");
        doc.draw(
            DrawContext::default(),
            0.0,
            -scroll,
            Some(Position { x: 0.0, y: 0.0, width: w as f32, height: h as f32 }),
        );
        if !rects.is_empty() {
            self.with_container(|c| c.draw_selection_rects(&rects));
        }

        self.with_container(|c| f(c.width(), c.height(), c.pixels()))
    }

    /// Whether the mouse is over a link (for the cursor shape).
    pub fn hover_is_link(&mut self, x: f32, y: f32, scroll_y: u32) -> bool {
        let doc_y = y + scroll_y as f32;
        let doc = self.doc.as_mut().expect("document is present until drop");
        doc.on_mouse_over(x, doc_y, x, y);
        self.with_container(|c| c.cursor() == "pointer")
    }

    /// Returns the safe (http/https/mailto) target of the link under `(x, y)` viewport
    /// coordinates, if any.
    pub fn link_at(&mut self, x: f32, y: f32, scroll_y: u32) -> Option<String> {
        let doc_y = y + scroll_y as f32;
        let doc = self.doc.as_mut().expect("document is present until drop");
        // litehtml only arms a link on press when it already knows the pointer is over it.
        doc.on_mouse_over(x, doc_y, x, y);
        doc.on_lbutton_down(x, doc_y, x, y);
        doc.on_lbutton_up(x, doc_y, x, y);
        let url = self.with_container(|c| c.take_anchor_click())?;
        safe_link(&url)
    }

    /// Starts a text selection at viewport coordinates `(x, y)`.
    pub fn selection_start(&mut self, x: f32, y: f32, scroll_y: u32) {
        let doc_y = y + scroll_y as f32;
        let measure = self.with_container(|c| c.text_measure_fn());
        let doc = self.doc.as_ref().expect("document is present until drop");
        self.selection.start_at(doc, &measure, x, doc_y, x, y);
    }

    /// Extends the current selection to viewport coordinates `(x, y)`.
    pub fn selection_extend(&mut self, x: f32, y: f32, scroll_y: u32) {
        let doc_y = y + scroll_y as f32;
        let measure = self.with_container(|c| c.text_measure_fn());
        let doc = self.doc.as_ref().expect("document is present until drop");
        self.selection.extend_to(doc, &measure, x, doc_y, x, y);
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }

    /// The selected plain text, or `None` when nothing is selected.
    pub fn selected_text(&self) -> Option<String> {
        self.selection.selected_text().filter(|t| !t.is_empty())
    }
}

impl Drop for EmailView {
    fn drop(&mut self) {
        self.selection.clear();
        self.doc.take();
        // SAFETY: `container` came from `Box::into_raw` and the document borrowing it is gone.
        unsafe { drop(Box::from_raw(self.container)) };
    }
}

fn is_remote_url(url: &str) -> bool {
    let l = url.trim_start().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Only web and mail links may leave the application; everything else (file:, javascript:,
/// custom protocol handlers, ...) is refused.
pub fn safe_link(url: &str) -> Option<String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let ok = ["http://", "https://", "mailto:"].iter().any(|p| lower.starts_with(p));
    (ok && !url.chars().any(|c| c.is_control())).then(|| url.to_string())
}

/// Converts premultiplied RGBA into opaque RGBA by compositing over a white background.
fn flatten_on_white(premul: &[u8]) -> Vec<u8> {
    let mut out = vec![0; premul.len()];
    flatten_on_white_into(premul, &mut out);
    out
}

/// Composite premultiplied pixels into an equally sized opaque RGBA buffer.
pub fn flatten_on_white_into(premul: &[u8], out: &mut [u8]) {
    assert_eq!(premul.len(), out.len());
    for (px, dst) in premul.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
        let inv = 255 - px[3] as u16;
        dst[0] = (px[0] as u16 + inv).min(255) as u8;
        dst[1] = (px[1] as u16 + inv).min(255) as u8;
        dst[2] = (px[2] as u16 + inv).min(255) as u8;
        dst[3] = 255;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(html: &str) -> ParsedEmail {
        ParsedEmail { html: Some(html.to_string()), ..Default::default() }
    }

    #[test]
    fn safe_link_filters_schemes() {
        assert!(safe_link("https://example.com/a?b=c").is_some());
        assert!(safe_link("MAILTO:a@b.c").is_some());
        assert!(safe_link("javascript:alert(1)").is_none());
        assert!(safe_link("file:///C:/Windows/System32/calc.exe").is_none());
        assert!(safe_link("ms-msdt:/id").is_none());
        assert!(safe_link("https://x.com/\u{0}").is_none());
    }

    #[test]
    fn flatten_composites_over_white() {
        assert_eq!(flatten_on_white(&[0, 0, 0, 0]), vec![255, 255, 255, 255]);
        assert_eq!(flatten_on_white(&[10, 20, 30, 255]), vec![10, 20, 30, 255]);
    }

    #[test]
    fn renders_text_into_opaque_viewport() {
        let mut v = EmailView::new(&email("<p>Hello world, this is a test message.</p>"), 400, 200, 1.0).unwrap();
        assert!(v.content_height() > 0);
        let f = v.render_viewport(0);
        assert_eq!((f.width, f.height), (400, 200));
        assert_eq!(f.rgba.len(), 400 * 200 * 4);
        assert!(f.rgba.chunks_exact(4).all(|p| p[3] == 255));
        assert!(f.rgba.chunks_exact(4).any(|p| p[0] < 128), "expected dark text pixels");
        let direct = v.render_viewport_with(0, |w, h, premul| {
            let mut output = vec![0; (w * h * 4) as usize];
            flatten_on_white_into(premul, &mut output);
            output
        });
        assert_eq!(f.rgba, direct);
    }

    #[test]
    fn scrolling_changes_the_frame_and_does_not_accumulate() {
        let body: String = (0..80).map(|i| format!("<p>Line number {i}</p>")).collect();
        let mut v = EmailView::new(&email(&body), 300, 100, 1.0).unwrap();
        assert!(v.content_height() > 300);
        let a = v.render_viewport(0).rgba;
        let b = v.render_viewport(120).rgba;
        let a2 = v.render_viewport(0).rgba;
        assert_ne!(a, b);
        assert_eq!(a, a2);
    }

    #[test]
    fn resize_reuses_document_and_matches_fresh_layout() {
        let body = email("<p>A long line of text that should wrap differently in a narrow viewport.</p>");
        let mut view = EmailView::new(&body, 350, 150, 1.0).unwrap();
        view.resize(180, 90);
        let mut fresh = EmailView::new(&body, 180, 90, 1.0).unwrap();
        assert_eq!(view.content_height(), fresh.content_height());
        assert_eq!(view.render_viewport(0).rgba, fresh.render_viewport(0).rgba);
        view.resize(350, 150);
        let mut fresh = EmailView::new(&body, 350, 150, 1.0).unwrap();
        assert_eq!(view.render_viewport(0).rgba, fresh.render_viewport(0).rgba);
    }

    #[test]
    fn link_hit_testing_returns_href_and_ignores_unsafe_schemes() {
        let html = "<p><a href=\"https://example.com/x\">Click this long link text</a></p>\
                    <p style=\"margin-top:40px\"><a href=\"javascript:evil()\">Bad link text here</a></p>";
        let mut v = EmailView::new(&email(html), 400, 300, 1.0).unwrap();
        assert!(v.hover_is_link(10.0, 8.0, 0));
        assert_eq!(v.link_at(10.0, 8.0, 0).as_deref(), Some("https://example.com/x"));
        assert_eq!(v.link_at(390.0, 250.0, 0), None);
    }

    #[test]
    fn link_click_works_without_a_prior_hover() {
        let html = "<p><a href=\"https://example.com/y\">Open me</a></p>";
        let mut v = EmailView::new(&email(html), 400, 300, 1.0).unwrap();
        assert_eq!(v.link_at(10.0, 8.0, 0).as_deref(), Some("https://example.com/y"));
    }

    #[test]
    fn drag_selection_copies_text_between_points() {
        let html = "<p>Alpha beta gamma delta epsilon</p>";
        let mut v = EmailView::new(&email(html), 400, 200, 1.0).unwrap();
        assert_eq!(v.selected_text(), None);
        v.selection_start(2.0, 8.0, 0);
        v.selection_extend(200.0, 8.0, 0);
        let text = v.selected_text().expect("selection should contain text");
        assert!(text.contains("Alpha"), "selected: {text:?}");
        let f = v.render_viewport(0);
        assert!(f.rgba.chunks_exact(4).all(|p| p[3] == 255));
        v.clear_selection();
        assert_eq!(v.selected_text(), None);
    }

    #[test]
    fn remote_images_are_reported_but_blocked_until_provided() {
        let html = "<p>hi</p><img src=\"https://tracker.example/p.gif\" width=\"5\" height=\"5\">";
        let mut v = EmailView::new(&email(html), 300, 100, 1.0).unwrap();
        assert_eq!(v.remote_images(), ["https://tracker.example/p.gif"]);
        assert_eq!(v.blocked_remote_images(), 1);
        v.load_remote_images(vec![("https://tracker.example/p.gif".to_string(), Vec::new())]);
        assert_eq!(v.blocked_remote_images(), 0);
        v.resize(200, 80);
        assert_eq!(v.blocked_remote_images(), 0);
    }

    #[test]
    fn images_are_scaled_to_their_laid_out_size() {
        // 40x40 solid red PNG shown in a 10x10 box must not spill outside it.
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(40, 40, image::Rgb([255, 0, 0]))
            .write_to(&mut bmp, image::ImageFormat::Png)
            .unwrap();
        let bmp = bmp.into_inner();
        let html = "<img src=\"https://x.example/a.bmp\" width=\"10\" height=\"10\">";
        let mut v = EmailView::new(&email(html), 100, 60, 1.0).unwrap();
        v.load_remote_images(vec![("https://x.example/a.bmp".to_string(), bmp)]);
        let f = v.render_viewport(0);
        let red = |x: usize, y: usize| {
            let i = (y * 100 + x) * 4;
            f.rgba[i] > 200 && f.rgba[i + 1] < 60
        };
        assert!(red(5, 5));
        assert!(!red(20, 20), "image drawn at natural size instead of scaled");
    }
}
