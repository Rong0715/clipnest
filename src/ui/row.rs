//! List rows: `ItemObject` (model data for one history row) and `RowWidget` (its view).

use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, pango};

mod obj_imp {
    use super::*;

    #[derive(Default)]
    pub struct ItemObject {
        pub id: Cell<i64>,
        pub markup: RefCell<String>,
        pub shortcut: RefCell<String>,
        pub color: Cell<Option<[f64; 4]>>,
        pub is_image: Cell<bool>,
        pub thumb: RefCell<Option<gdk::Texture>>,
        pub thumb_failed: Cell<bool>,
        pub app_icon: RefCell<Option<gio::Icon>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ItemObject {
        const NAME: &'static str = "ClipnestItemObject";
        type Type = super::ItemObject;
    }

    impl ObjectImpl for ItemObject {}
}

glib::wrapper! {
    pub struct ItemObject(ObjectSubclass<obj_imp::ItemObject>);
}

impl ItemObject {
    pub fn new(id: i64) -> ItemObject {
        let obj: ItemObject = glib::Object::new();
        obj.imp().id.set(id);
        obj
    }
    pub fn id(&self) -> i64 {
        self.imp().id.get()
    }
    pub fn set_display(&self, markup: String, shortcut: String, color: Option<[f64; 4]>, is_image: bool) {
        let imp = self.imp();
        *imp.markup.borrow_mut() = markup;
        *imp.shortcut.borrow_mut() = shortcut;
        imp.color.set(color);
        imp.is_image.set(is_image);
    }
    pub fn set_shortcut(&self, shortcut: String) {
        *self.imp().shortcut.borrow_mut() = shortcut;
    }
    pub fn set_app_icon(&self, icon: Option<gio::Icon>) {
        *self.imp().app_icon.borrow_mut() = icon;
    }
    pub fn thumb(&self) -> Option<gdk::Texture> {
        self.imp().thumb.borrow().clone()
    }
    pub fn set_thumb(&self, texture: Option<gdk::Texture>) {
        self.imp().thumb_failed.set(texture.is_none());
        *self.imp().thumb.borrow_mut() = texture;
    }
    pub fn thumb_failed(&self) -> bool {
        self.imp().thumb_failed.get()
    }
    pub fn clear_thumb(&self) {
        *self.imp().thumb.borrow_mut() = None;
        self.imp().thumb_failed.set(false);
    }
    pub fn is_image(&self) -> bool {
        self.imp().is_image.get()
    }
}

mod row_imp {
    use super::*;

    #[derive(Default)]
    pub struct RowWidget {
        pub icon: gtk::Image,
        pub swatch: gtk::DrawingArea,
        pub thumb: gtk::Picture,
        pub title: gtk::Label,
        pub shortcut: gtk::Label,
        pub color: Cell<Option<[f64; 4]>>,
        /// Item id (or footer index) this row shows.
        pub key: Cell<i64>,
        /// Position in its list, refreshed on bind.
        pub position: Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RowWidget {
        const NAME: &'static str = "ClipnestRowWidget";
        type Type = super::RowWidget;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for RowWidget {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Horizontal);
            obj.set_spacing(6);
            obj.add_css_class("clipnest-row");

            self.icon.set_pixel_size(16);
            self.icon.set_visible(false);
            obj.append(&self.icon);

            self.swatch.set_content_width(12);
            self.swatch.set_content_height(12);
            self.swatch.set_valign(gtk::Align::Center);
            self.swatch.set_visible(false);
            let weak = obj.downgrade();
            self.swatch.set_draw_func(move |_, cr, w, h| {
                let Some(row) = weak.upgrade() else { return };
                if let Some([r, g, b, a]) = row.imp().color.get() {
                    cr.rectangle(0.5, 0.5, w as f64 - 1.0, h as f64 - 1.0);
                    cr.set_source_rgba(r, g, b, a);
                    let _ = cr.fill_preserve();
                    cr.set_source_rgba(0.5, 0.5, 0.5, 0.6);
                    cr.set_line_width(1.0);
                    let _ = cr.stroke();
                }
            });
            obj.append(&self.swatch);

            self.thumb.set_visible(false);
            self.thumb.set_halign(gtk::Align::Start);
            self.thumb.set_can_shrink(true);
            self.thumb.set_content_fit(gtk::ContentFit::Contain);
            obj.append(&self.thumb);

            self.title.set_xalign(0.0);
            self.title.set_hexpand(true);
            self.title.set_single_line_mode(true);
            self.title.set_ellipsize(pango::EllipsizeMode::Middle);
            self.title.add_css_class("clipnest-title");
            obj.append(&self.title);

            self.shortcut.set_xalign(1.0);
            self.shortcut.add_css_class("clipnest-shortcut");
            obj.append(&self.shortcut);
        }
    }

    impl WidgetImpl for RowWidget {}
    impl BoxImpl for RowWidget {}
}

glib::wrapper! {
    pub struct RowWidget(ObjectSubclass<row_imp::RowWidget>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for RowWidget {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl RowWidget {
    pub fn new() -> RowWidget {
        RowWidget::default()
    }

    pub fn key(&self) -> i64 {
        self.imp().key.get()
    }

    pub fn position(&self) -> u32 {
        self.imp().position.get()
    }

    pub fn set_position(&self, position: u32) {
        self.imp().position.set(position);
    }

    /// Shows a history item.
    pub fn bind_item(&self, obj: &ItemObject, position: u32, thumb_height: i32) {
        let imp = self.imp();
        imp.key.set(obj.id());
        imp.position.set(position);
        imp.color.set(obj.imp().color.get());
        imp.swatch.set_visible(imp.color.get().is_some());
        imp.swatch.queue_draw();
        match obj.imp().app_icon.borrow().as_ref() {
            Some(icon) => {
                imp.icon.set_from_gicon(icon);
                imp.icon.set_visible(true);
            }
            None => imp.icon.set_visible(false),
        }
        if obj.is_image() {
            let texture = obj.thumb();
            imp.thumb.set_paintable(texture.as_ref());
            imp.thumb.set_visible(true);
            if let Some(t) = texture {
                let h = thumb_height.max(1);
                let w = ((t.width() as f64 / t.height().max(1) as f64) * h as f64).round() as i32;
                imp.thumb.set_size_request(w.clamp(1, 340), h);
            } else {
                imp.thumb.set_size_request(thumb_height.max(1), thumb_height.max(1));
            }
            let markup = obj.imp().markup.borrow();
            imp.title.set_visible(!markup.is_empty());
            imp.title.set_markup(&markup);
        } else {
            imp.thumb.set_paintable(gdk::Paintable::NONE);
            imp.thumb.set_visible(false);
            imp.title.set_visible(true);
            imp.title.set_markup(&obj.imp().markup.borrow());
        }
        imp.shortcut.set_label(&obj.imp().shortcut.borrow());
    }

    pub fn set_thumb(&self, texture: &gdk::Texture, thumb_height: i32) {
        let imp = self.imp();
        let h = thumb_height.max(1);
        let w = ((texture.width() as f64 / texture.height().max(1) as f64) * h as f64).round() as i32;
        imp.thumb.set_size_request(w.clamp(1, 340), h);
        imp.thumb.set_paintable(Some(texture));
    }

    /// Shows a footer entry ("Clear", "Preferences…", …).
    pub fn bind_text(&self, key: i64, title: &str, shortcut: &str) {
        let imp = self.imp();
        imp.key.set(key);
        imp.icon.set_visible(false);
        imp.swatch.set_visible(false);
        imp.thumb.set_visible(false);
        imp.title.set_visible(true);
        imp.title.set_text(title);
        imp.shortcut.set_label(shortcut);
    }

    pub fn set_title(&self, title: &str) {
        self.imp().title.set_text(title);
    }

    pub fn set_shortcut(&self, shortcut: &str) {
        self.imp().shortcut.set_label(shortcut);
    }
}
