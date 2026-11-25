//! Pure-Rust `canvas` vector icon set for `nsh-music`.
//!
//! Every glyph is drawn in a crisp `24 × 24` coordinate box using NSH's
//! [`Painter`] with rounded caps/joins and reactive [`AdaptiveColor`] tinting.

use std::f32::consts::{PI, TAU};

use nsh_core::{
    canvas::{LineCap, LineJoin},
    geometry::{point, size},
    prelude::*,
};

/// All UI vector icons supported by `nsh-music`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    Next,
    Prev,
    Shuffle,
    Repeat,
    RepeatOne,
    Heart,
    HeartFilled,
    Search,
    Home,
    Compass,
    Library,
    Radio,
    Podcast,
    Settings,
    Plugin,
    Queue,
    Lyrics,
    Comment,
    Volume,
    VolumeMute,
    ChevronLeft,
    ChevronRight,
    ChevronDown,
    Menu,
    Plus,
    Check,
    Close,
    External,
    Vinyl,
    Cassette,
    Disc,
    Sparkles,
    Download,
    QrCode,
    Globe,
    MoreHorizontal,
    Sidebar,
    Expand,
    Shrink,
    User,
    Folder,
    Trash,
    Refresh,
    ArrowUp,
    ArrowDown,
    Cast,
}

nsh_core::static_reactive!(Icon: Icon);

fn pt(x: f32, y: f32) -> Point<Points> {
    point(px(x), px(y))
}

fn rect_bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Points> {
    Bounds::new(pt(x, y), size(px(w), px(h)))
}

fn polyline(p: Painter, coords: &[f32]) -> Painter {
    if coords.len() < 4 {
        return p;
    }
    let mut p = p.begin_path().move_to(coords[0], coords[1]);
    for [x, y] in coords[2..].as_chunks::<2>().0 {
        p = p.line_to(*x, *y);
    }
    p.stroke()
}

fn stroke_round_rect(p: Painter, x: f32, y: f32, w: f32, h: f32, r: f32) -> Painter {
    p.begin_path()
        .round_rect(rect_bounds(x, y, w, h), px(r))
        .stroke()
}

fn fill_round_rect(p: Painter, x: f32, y: f32, w: f32, h: f32, r: f32) -> Painter {
    p.begin_path()
        .round_rect(rect_bounds(x, y, w, h), px(r))
        .fill()
}

fn circle(p: Painter, cx: f32, cy: f32, r: f32, filled: bool) -> Painter {
    let p = p.begin_path().arc(cx, cy, r, 0.0, TAU);
    if filled { p.fill() } else { p.stroke() }
}

impl Icon {
    /// Draws this icon into a `24 × 24` logical viewport.
    pub fn draw(self, p: Painter) -> Painter {
        match self {
            Icon::Play => p
                .begin_path()
                .move_to(7.5, 4.8)
                .line_to(19.2, 12.0)
                .line_to(7.5, 19.2)
                .close_path()
                .fill(),

            Icon::Pause => {
                let p = fill_round_rect(p, 6.2, 4.8, 4.2, 14.4, 1.4);
                fill_round_rect(p, 13.6, 4.8, 4.2, 14.4, 1.4)
            }

            Icon::Next => {
                let p = p
                    .begin_path()
                    .move_to(5.5, 5.5)
                    .line_to(14.5, 12.0)
                    .line_to(5.5, 18.5)
                    .close_path()
                    .fill();
                fill_round_rect(p, 16.5, 5.5, 2.4, 13.0, 1.0)
            }

            Icon::Prev => {
                let p = fill_round_rect(p, 5.1, 5.5, 2.4, 13.0, 1.0);
                p.begin_path()
                    .move_to(18.5, 5.5)
                    .line_to(9.5, 12.0)
                    .line_to(18.5, 18.5)
                    .close_path()
                    .fill()
            }

            Icon::Shuffle => {
                let p = polyline(p, &[3.5, 7.0, 8.0, 7.0, 15.5, 17.0, 20.0, 17.0]);
                let p = polyline(p, &[17.2, 14.2, 20.0, 17.0, 17.2, 19.8]);
                let p = polyline(p, &[3.5, 17.0, 8.0, 17.0, 10.6, 13.6]);
                let p = polyline(p, &[13.4, 10.4, 15.5, 7.0, 20.0, 7.0]);
                polyline(p, &[17.2, 4.2, 20.0, 7.0, 17.2, 9.8])
            }

            Icon::Repeat => {
                let p = polyline(p, &[17.0, 3.8, 20.0, 6.8, 17.0, 9.8]);
                let p = polyline(p, &[4.0, 11.5, 4.0, 8.8, 6.0, 6.8, 20.0, 6.8]);
                let p = polyline(p, &[7.0, 20.2, 4.0, 17.2, 7.0, 14.2]);
                polyline(p, &[20.0, 12.5, 20.0, 15.2, 18.0, 17.2, 4.0, 17.2])
            }

            Icon::RepeatOne => {
                let p = polyline(p, &[17.0, 3.8, 20.0, 6.8, 17.0, 9.8]);
                let p = polyline(p, &[4.0, 11.2, 4.0, 8.8, 6.0, 6.8, 20.0, 6.8]);
                let p = polyline(p, &[7.0, 20.2, 4.0, 17.2, 7.0, 14.2]);
                let p = polyline(p, &[20.0, 12.8, 20.0, 15.2, 18.0, 17.2, 4.0, 17.2]);
                // Small "1" centered inside the loop
                polyline(p, &[11.0, 10.4, 12.3, 9.4, 12.3, 14.6])
            }

            Icon::Heart => p
                .begin_path()
                .move_to(12.0, 19.8)
                .bezier_curve_to(pt(5.2, 15.2), pt(3.2, 11.4), pt(3.5, 8.4))
                .bezier_curve_to(pt(3.8, 5.6), pt(6.1, 4.2), pt(8.5, 4.5))
                .bezier_curve_to(pt(10.2, 4.7), pt(11.4, 5.8), pt(12.0, 7.2))
                .bezier_curve_to(pt(12.6, 5.8), pt(13.8, 4.7), pt(15.5, 4.5))
                .bezier_curve_to(pt(17.9, 4.2), pt(20.2, 5.6), pt(20.5, 8.4))
                .bezier_curve_to(pt(20.8, 11.4), pt(18.8, 15.2), pt(12.0, 19.8))
                .close_path()
                .stroke(),

            Icon::HeartFilled => p
                .begin_path()
                .move_to(12.0, 20.0)
                .bezier_curve_to(pt(5.0, 15.4), pt(3.0, 11.4), pt(3.3, 8.3))
                .bezier_curve_to(pt(3.6, 5.4), pt(6.0, 4.0), pt(8.5, 4.3))
                .bezier_curve_to(pt(10.2, 4.5), pt(11.4, 5.7), pt(12.0, 7.1))
                .bezier_curve_to(pt(12.6, 5.7), pt(13.8, 4.5), pt(15.5, 4.3))
                .bezier_curve_to(pt(18.0, 4.0), pt(20.4, 5.4), pt(20.7, 8.3))
                .bezier_curve_to(pt(21.0, 11.4), pt(19.0, 15.4), pt(12.0, 20.0))
                .close_path()
                .fill(),

            Icon::Search => {
                let p = circle(p, 10.6, 10.6, 6.2, false);
                polyline(p, &[15.2, 15.2, 20.2, 20.2])
            }

            Icon::Home => {
                let p = polyline(p, &[3.8, 10.8, 12.0, 4.0, 20.2, 10.8]);
                let p = polyline(p, &[5.8, 9.6, 5.8, 19.5, 18.2, 19.5, 18.2, 9.6]);
                polyline(p, &[9.8, 19.5, 9.8, 13.8, 14.2, 13.8, 14.2, 19.5])
            }

            Icon::Compass => {
                let p = circle(p, 12.0, 12.0, 8.5, false);
                p.begin_path()
                    .move_to(15.6, 8.4)
                    .line_to(13.4, 13.4)
                    .line_to(8.4, 15.6)
                    .line_to(10.6, 10.6)
                    .close_path()
                    .stroke()
            }

            Icon::Library => {
                let p = polyline(p, &[5.0, 5.0, 5.0, 19.0]);
                let p = polyline(p, &[9.5, 5.0, 9.5, 19.0]);
                let p = polyline(p, &[13.8, 5.6, 18.5, 19.0]);
                polyline(p, &[3.5, 19.0, 20.5, 19.0])
            }

            Icon::Radio => {
                let p = circle(p, 12.0, 12.0, 1.8, true);
                let p = p
                    .begin_path()
                    .arc(12.0, 12.0, 5.0, -PI * 0.35, PI * 0.35)
                    .stroke();
                let p = p
                    .begin_path()
                    .arc(12.0, 12.0, 5.0, PI * 0.65, PI * 1.35)
                    .stroke();
                let p = p
                    .begin_path()
                    .arc(12.0, 12.0, 8.5, -PI * 0.35, PI * 0.35)
                    .stroke();
                p.begin_path()
                    .arc(12.0, 12.0, 8.5, PI * 0.65, PI * 1.35)
                    .stroke()
            }

            Icon::Podcast => {
                let p = circle(p, 12.0, 10.5, 1.8, true);
                let p = p
                    .begin_path()
                    .arc(12.0, 10.5, 5.2, PI * 0.78, PI * 2.22)
                    .stroke();
                let p = p
                    .begin_path()
                    .arc(12.0, 10.5, 8.6, PI * 0.82, PI * 2.18)
                    .stroke();
                p.begin_path()
                    .move_to(12.0, 13.0)
                    .line_to(9.8, 20.2)
                    .line_to(14.2, 20.2)
                    .close_path()
                    .stroke()
            }

            Icon::Settings => {
                let p = circle(p, 12.0, 12.0, 3.2, false);
                (0..8).fold(p, |p, i| {
                    let a = i as f32 * PI / 4.0;
                    let (sin, cos) = a.sin_cos();
                    polyline(
                        p,
                        &[
                            12.0 + cos * 6.2,
                            12.0 + sin * 6.2,
                            12.0 + cos * 9.0,
                            12.0 + sin * 9.0,
                        ],
                    )
                })
            }

            Icon::Plugin => {
                let p = polyline(p, &[9.0, 3.8, 9.0, 7.5]);
                let p = polyline(p, &[15.0, 3.8, 15.0, 7.5]);
                let p = stroke_round_rect(p, 6.2, 7.5, 11.6, 6.8, 3.0);
                polyline(p, &[12.0, 14.3, 12.0, 20.2])
            }

            Icon::Queue => {
                let p = polyline(p, &[4.0, 6.5, 19.5, 6.5]);
                let p = polyline(p, &[4.0, 11.5, 15.5, 11.5]);
                let p = polyline(p, &[4.0, 16.5, 12.5, 16.5]);
                p.begin_path()
                    .move_to(16.0, 14.0)
                    .line_to(20.5, 16.5)
                    .line_to(16.0, 19.0)
                    .close_path()
                    .fill()
            }

            Icon::Lyrics => {
                let p = polyline(p, &[4.5, 6.0, 19.5, 6.0]);
                let p = polyline(p, &[4.5, 10.5, 16.0, 10.5]);
                let p = polyline(p, &[4.5, 15.0, 12.0, 15.0]);
                let p = polyline(p, &[16.5, 18.5, 16.5, 13.0, 20.0, 12.2]);
                circle(p, 15.2, 18.5, 1.6, true)
            }

            Icon::Comment => {
                let p = p
                    .begin_path()
                    .move_to(6.2, 19.8)
                    .line_to(6.5, 16.5)
                    .line_to(5.0, 16.5)
                    .bezier_curve_to(pt(3.8, 16.5), pt(3.5, 15.5), pt(3.5, 14.5))
                    .line_to(3.5, 6.8)
                    .bezier_curve_to(pt(3.5, 5.2), pt(4.8, 4.5), pt(6.0, 4.5))
                    .line_to(18.0, 4.5)
                    .bezier_curve_to(pt(19.5, 4.5), pt(20.5, 5.5), pt(20.5, 6.8))
                    .line_to(20.5, 14.5)
                    .bezier_curve_to(pt(20.5, 15.8), pt(19.5, 16.5), pt(18.0, 16.5))
                    .line_to(10.5, 16.5)
                    .close_path()
                    .stroke();
                let p = polyline(p, &[7.8, 9.0, 16.2, 9.0]);
                polyline(p, &[7.8, 12.4, 13.2, 12.4])
            }

            Icon::Volume => {
                let p = p
                    .begin_path()
                    .move_to(4.0, 9.5)
                    .line_to(8.0, 9.5)
                    .line_to(12.6, 5.6)
                    .line_to(12.6, 18.4)
                    .line_to(8.0, 14.5)
                    .line_to(4.0, 14.5)
                    .close_path()
                    .stroke();
                let p = p
                    .begin_path()
                    .arc(13.5, 12.0, 3.6, -PI * 0.34, PI * 0.34)
                    .stroke();
                p.begin_path()
                    .arc(13.5, 12.0, 6.4, -PI * 0.36, PI * 0.36)
                    .stroke()
            }

            Icon::VolumeMute => {
                let p = p
                    .begin_path()
                    .move_to(4.0, 9.5)
                    .line_to(8.0, 9.5)
                    .line_to(12.6, 5.6)
                    .line_to(12.6, 18.4)
                    .line_to(8.0, 14.5)
                    .line_to(4.0, 14.5)
                    .close_path()
                    .stroke();
                let p = polyline(p, &[16.0, 9.2, 20.6, 14.8]);
                polyline(p, &[20.6, 9.2, 16.0, 14.8])
            }

            Icon::ChevronLeft => polyline(p, &[15.0, 5.5, 8.5, 12.0, 15.0, 18.5]),
            Icon::ChevronRight => polyline(p, &[9.0, 5.5, 15.5, 12.0, 9.0, 18.5]),
            Icon::ChevronDown => polyline(p, &[5.5, 9.0, 12.0, 15.5, 18.5, 9.0]),

            Icon::Menu => {
                let p = polyline(p, &[4.0, 6.5, 20.0, 6.5]);
                let p = polyline(p, &[4.0, 12.0, 20.0, 12.0]);
                polyline(p, &[4.0, 17.5, 20.0, 17.5])
            }

            Icon::Plus => {
                let p = polyline(p, &[12.0, 5.0, 12.0, 19.0]);
                polyline(p, &[5.0, 12.0, 19.0, 12.0])
            }

            Icon::Check => polyline(p, &[4.8, 12.6, 9.6, 17.4, 19.2, 7.0]),

            Icon::Close => {
                let p = polyline(p, &[6.0, 6.0, 18.0, 18.0]);
                polyline(p, &[18.0, 6.0, 6.0, 18.0])
            }

            Icon::External => {
                let p = polyline(p, &[14.0, 4.5, 19.5, 4.5, 19.5, 10.0]);
                let p = polyline(p, &[11.0, 13.0, 19.5, 4.5]);
                polyline(
                    p,
                    &[17.5, 14.0, 17.5, 19.0, 5.0, 19.0, 5.0, 6.5, 10.0, 6.5],
                )
            }

            Icon::Vinyl => {
                let p = circle(p, 12.0, 12.0, 8.8, false);
                let p = p
                    .begin_path()
                    .arc(12.0, 12.0, 5.8, -PI * 0.15, PI * 0.45)
                    .stroke();
                let p = p
                    .begin_path()
                    .arc(12.0, 12.0, 5.8, PI * 0.85, PI * 1.45)
                    .stroke();
                let p = circle(p, 12.0, 12.0, 2.8, false);
                circle(p, 12.0, 12.0, 0.9, true)
            }

            Icon::Cassette => {
                let p = stroke_round_rect(p, 3.0, 5.5, 18.0, 13.0, 2.5);
                let p = circle(p, 8.2, 11.4, 2.0, false);
                let p = circle(p, 15.8, 11.4, 2.0, false);
                let p = polyline(p, &[8.2, 13.4, 15.8, 13.4]);
                polyline(p, &[6.8, 18.5, 8.2, 16.0, 15.8, 16.0, 17.2, 18.5])
            }

            Icon::Disc => {
                let p = circle(p, 12.0, 12.0, 8.6, false);
                let p = circle(p, 12.0, 12.0, 2.5, false);
                polyline(p, &[12.0, 3.4, 12.0, 6.0])
            }

            Icon::Sparkles => {
                let p = p
                    .begin_path()
                    .move_to(11.0, 3.5)
                    .line_to(12.8, 9.2)
                    .line_to(18.5, 11.0)
                    .line_to(12.8, 12.8)
                    .line_to(11.0, 18.5)
                    .line_to(9.2, 12.8)
                    .line_to(3.5, 11.0)
                    .line_to(9.2, 9.2)
                    .close_path()
                    .stroke();
                let p = polyline(p, &[18.5, 15.5, 18.5, 20.5]);
                polyline(p, &[16.0, 18.0, 21.0, 18.0])
            }

            Icon::Download => {
                let p = polyline(p, &[12.0, 4.0, 12.0, 15.2]);
                let p = polyline(p, &[7.6, 11.0, 12.0, 15.4, 16.4, 11.0]);
                polyline(p, &[4.5, 16.5, 4.5, 19.5, 19.5, 19.5, 19.5, 16.5])
            }

            Icon::QrCode => {
                let p = stroke_round_rect(p, 3.8, 3.8, 6.2, 6.2, 1.0);
                let p = stroke_round_rect(p, 14.0, 3.8, 6.2, 6.2, 1.0);
                let p = stroke_round_rect(p, 3.8, 14.0, 6.2, 6.2, 1.0);
                let p = fill_round_rect(p, 5.9, 5.9, 2.0, 2.0, 0.4);
                let p = fill_round_rect(p, 16.1, 5.9, 2.0, 2.0, 0.4);
                let p = fill_round_rect(p, 5.9, 16.1, 2.0, 2.0, 0.4);
                let p = polyline(p, &[14.5, 14.5, 17.0, 14.5, 17.0, 17.0]);
                polyline(p, &[14.5, 19.8, 19.8, 19.8, 19.8, 14.5])
            }

            Icon::Globe => {
                let p = circle(p, 12.0, 12.0, 8.6, false);
                let p = polyline(p, &[3.6, 12.0, 20.4, 12.0]);
                let p = p
                    .begin_path()
                    .ellipse(
                        pt(12.0, 12.0),
                        size(px(4.2), px(8.6)),
                        Radians(0.0),
                        Radians(0.0),
                        Radians(TAU),
                        false,
                    )
                    .stroke();
                let p = polyline(p, &[5.4, 7.6, 18.6, 7.6]);
                polyline(p, &[5.4, 16.4, 18.6, 16.4])
            }

            Icon::MoreHorizontal => {
                let p = circle(p, 6.0, 12.0, 1.6, true);
                let p = circle(p, 12.0, 12.0, 1.6, true);
                circle(p, 18.0, 12.0, 1.6, true)
            }

            Icon::Sidebar => {
                let p = stroke_round_rect(p, 3.5, 4.5, 17.0, 15.0, 2.6);
                polyline(p, &[9.2, 4.5, 9.2, 19.5])
            }

            Icon::Expand => {
                let p = polyline(p, &[14.5, 4.5, 19.5, 4.5, 19.5, 9.5]);
                let p = polyline(p, &[19.5, 4.5, 13.5, 10.5]);
                let p = polyline(p, &[9.5, 19.5, 4.5, 19.5, 4.5, 14.5]);
                polyline(p, &[4.5, 19.5, 10.5, 13.5])
            }

            Icon::Shrink => {
                let p = polyline(p, &[14.0, 5.0, 14.0, 10.0, 19.0, 10.0]);
                let p = polyline(p, &[19.5, 4.5, 14.0, 10.0]);
                let p = polyline(p, &[10.0, 19.0, 10.0, 14.0, 5.0, 14.0]);
                polyline(p, &[4.5, 19.5, 10.0, 14.0])
            }

            Icon::User => {
                let p = circle(p, 12.0, 8.2, 3.6, false);
                p.begin_path()
                    .move_to(5.0, 19.8)
                    .quadratic_curve_to(pt(12.0, 14.2), pt(19.0, 19.8))
                    .stroke()
            }

            Icon::Folder => polyline(
                p,
                &[
                    3.8, 6.2, 9.5, 6.2, 11.4, 8.4, 20.2, 8.4, 20.2, 18.4, 3.8, 18.4, 3.8, 6.2,
                ],
            ),

            Icon::Trash => {
                let p = polyline(p, &[4.5, 6.5, 19.5, 6.5]);
                let p = polyline(p, &[9.2, 6.5, 9.2, 4.2, 14.8, 4.2, 14.8, 6.5]);
                let p = polyline(p, &[6.5, 6.5, 7.5, 19.5, 16.5, 19.5, 17.5, 6.5]);
                let p = polyline(p, &[10.2, 10.2, 10.4, 15.8]);
                polyline(p, &[13.8, 10.2, 13.6, 15.8])
            }

            Icon::Refresh => {
                let p = p
                    .begin_path()
                    .arc(12.0, 12.0, 7.8, 0.35, TAU - 0.55)
                    .stroke();
                polyline(p, &[16.5, 4.5, 19.8, 7.8, 15.8, 8.6])
            }

            Icon::ArrowUp => {
                let p = polyline(p, &[12.0, 19.0, 12.0, 5.2]);
                polyline(p, &[6.5, 10.6, 12.0, 5.0, 17.5, 10.6])
            }

            Icon::ArrowDown => {
                let p = polyline(p, &[12.0, 5.0, 12.0, 18.8]);
                polyline(p, &[6.5, 13.4, 12.0, 19.0, 17.5, 13.4])
            }

            Icon::Cast => {
                let p = polyline(p, &[4.0, 9.0, 4.0, 5.5, 20.0, 5.5, 20.0, 18.5, 13.0, 18.5]);
                let p = p
                    .begin_path()
                    .arc(4.2, 18.8, 4.2, -1.55, 0.0)
                    .stroke();
                let p = p
                    .begin_path()
                    .arc(4.2, 18.8, 7.6, -1.55, 0.0)
                    .stroke();
                circle(p, 4.5, 18.5, 1.3, true)
            }
        }
    }
}

/// Renders a static vector [`Icon`] at `size` points tinted with `tint`.
pub fn icon(kind: Icon, icon_size: f32, tint: AdaptiveColor) -> View {
    icon_reactive(kind, icon_size, tint)
}

/// Renders a vector [`Icon`] whose glyph and [`AdaptiveColor`] tint can both
/// update reactively via signals or closures.
pub fn icon_reactive(
    kind: impl IntoReactive<Icon>,
    icon_size: f32,
    tint: impl IntoReactive<AdaptiveColor>,
) -> View {
    let kind_rx = kind.into_reactive();
    let tint_rx = tint.into_reactive();
    let scale = (icon_size / 24.0).max(0.1);
    view()
        .size(px(icon_size))
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .pointer_events(PointerEvents::None)
        .child(
            canvas(move || {
                let resolved = color(tint_rx.get());
                let painter = Painter::new()
                    .scale(scale, scale)
                    .stroke_color(resolved)
                    .fill_color(resolved)
                    .line_width(1.75)
                    .line_cap(LineCap::Round)
                    .line_join(LineJoin::Round);
                kind_rx.get().draw(painter)
            })
            .size_full(),
        )
}
