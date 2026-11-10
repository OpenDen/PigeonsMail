//! MIME interpretation and email HTML rendering.
pub mod mime;
pub mod view;

pub use mime::{
    extract_attachment, parse_email, preview, quote_source, AttachmentMeta, ParsedEmail, Preview,
    QuoteSource,
};
pub use view::{safe_link, EmailView, Frame};
