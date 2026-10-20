#[derive(Clone, Copy, Debug)]
pub struct LayoutRects {
    pub screen: egui::Rect,
    pub header: egui::Rect,
    pub sidebar: Option<egui::Rect>,
    pub projectbar: egui::Rect,
    pub worktop: egui::Rect,
    pub content: egui::Rect,
    pub composer: egui::Rect,
    pub monitor: Option<egui::Rect>,
    pub footer: egui::Rect,
    pub mobile: bool,
    pub monitor_is_drawer: bool,
}

impl LayoutRects {
    pub fn new(
        screen: egui::Rect,
        collapsed: bool,
        composer_focused: bool,
        has_composer: bool,
        scale: f32,
    ) -> Self {
        use egui::{Rect, pos2};
        let width = screen.width();
        let mobile = width <= 760.0;
        let monitor_is_drawer = width <= 1050.0;
        let header_h = if mobile { 46.0 } else { 48.0 };
        let project_h = if mobile {
            72.0
        } else if monitor_is_drawer {
            78.0
        } else {
            84.0
        };
        let worktop_h = if mobile { 44.0 } else { 48.0 };
        let footer_h = if mobile { 22.0 } else { 24.0 };
        let left_w = if mobile {
            0.0
        } else if width <= 1250.0 {
            196.0
        } else {
            220.0
        };
        let right_w = if monitor_is_drawer || collapsed {
            0.0
        } else if width <= 1250.0 {
            274.0
        } else {
            312.0
        };
        let composer_h = if has_composer {
            // Font growth gets more vertical space; the content's origin and scroll identity stay fixed.
            let extra = ((scale.clamp(1.0, 2.0) - 1.0) * 56.0).ceil();
            if mobile {
                (if composer_focused { 151.0 } else { 123.0 }) + extra
            } else {
                (if composer_focused { 174.0 } else { 136.0 }) + extra
            }
        } else {
            0.0
        };
        let header = Rect::from_min_max(screen.min, pos2(screen.right(), screen.top() + header_h));
        let footer =
            Rect::from_min_max(pos2(screen.left(), screen.bottom() - footer_h), screen.max);
        let sidebar = (left_w > 0.0).then(|| {
            Rect::from_min_max(
                pos2(screen.left(), header.bottom()),
                pos2(screen.left() + left_w, footer.top()),
            )
        });
        let projectbar = Rect::from_min_max(
            pos2(screen.left() + left_w, header.bottom()),
            pos2(screen.right(), header.bottom() + project_h),
        );
        let center_right = screen.right() - right_w;
        let worktop = Rect::from_min_max(
            pos2(projectbar.left(), projectbar.bottom()),
            pos2(center_right, projectbar.bottom() + worktop_h),
        );
        // A tiny window retains a nonnegative viewport and lets body content scroll.
        let composer_top = (footer.top() - composer_h).max(worktop.bottom());
        let content = Rect::from_min_max(worktop.left_bottom(), pos2(center_right, composer_top));
        let composer = Rect::from_min_max(
            pos2(projectbar.left(), composer_top),
            pos2(center_right, footer.top().max(composer_top)),
        );
        let monitor = (right_w > 0.0).then(|| {
            Rect::from_min_max(
                pos2(center_right, projectbar.bottom()),
                pos2(screen.right(), footer.top()),
            )
        });
        Self {
            screen,
            header,
            sidebar,
            projectbar,
            worktop,
            content,
            composer,
            monitor,
            footer,
            mobile,
            monitor_is_drawer,
        }
    }

    pub fn left_drawer(self) -> egui::Rect {
        egui::Rect::from_min_max(
            egui::pos2(self.screen.left(), self.header.bottom()),
            egui::pos2(
                self.screen.left() + 260.0_f32.min(self.screen.width() - 32.0),
                self.footer.top(),
            ),
        )
    }

    pub fn right_drawer(self) -> egui::Rect {
        let width = if self.mobile {
            330.0_f32.min(self.screen.width() - 32.0)
        } else {
            310.0
        };
        egui::Rect::from_min_max(
            egui::pos2(self.screen.right() - width, self.header.bottom()),
            egui::pos2(self.screen.right(), self.footer.top()),
        )
    }
}
