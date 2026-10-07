use crate::shared::theme;
use gpui::prelude::*;
use gpui::*;
use qrcode::QrCode;

pub fn qr_block(payload: &str, cell: f32) -> impl IntoElement {
    let Ok(code) = QrCode::new(payload.as_bytes()) else {
        return div()
            .text_sm()
            .text_color(theme::muted())
            .child("QR unavailable")
            .into_any();
    };
    let w = code.width();
    let modules = code.to_colors();
    let dark = theme::ink_on_plate();
    let light = theme::plate();
    div()
        .flex()
        .flex_col()
        .p_2()
        .bg(light)
        .rounded(px(6.0))
        .children((0..w).map(move |y| {
            let row = modules.clone();
            div().flex().children((0..w).map(move |x| {
                let on = row.get(y * w + x).copied() == Some(qrcode::Color::Dark);
                div()
                    .w(px(cell))
                    .h(px(cell))
                    .bg(if on { dark } else { light })
            }))
        }))
        .into_any()
}
