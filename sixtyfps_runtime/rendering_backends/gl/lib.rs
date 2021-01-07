/* LICENSE BEGIN
    This file is part of the SixtyFPS Project -- https://sixtyfps.io
    Copyright (c) 2020 Olivier Goffart <olivier.goffart@sixtyfps.io>
    Copyright (c) 2020 Simon Hausmann <simon.hausmann@sixtyfps.io>

    SPDX-License-Identifier: GPL-3.0-only
    This file is also available under commercial licensing terms.
    Please contact info@sixtyfps.io for more information.
LICENSE END */

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
};

use sixtyfps_corelib::graphics::{
    Color, Font, FontRequest, GraphicsBackend, Point, Rect, RenderingCache, Resource,
};
use sixtyfps_corelib::item_rendering::{CachedRenderingData, ItemRenderer};
use sixtyfps_corelib::items::Item;
use sixtyfps_corelib::window::ComponentWindow;
use sixtyfps_corelib::{SharedString, SharedVector};

mod graphics_window;
use graphics_window::*;
pub(crate) mod eventloop;

type CanvasRc = Rc<RefCell<femtovg::Canvas<femtovg::renderer::OpenGl>>>;
type ItemRenderingCacheRc = Rc<RefCell<RenderingCache<Option<GPUCachedData>>>>;

struct CachedImage {
    id: femtovg::ImageId,
    canvas: CanvasRc,
}

impl Drop for CachedImage {
    fn drop(&mut self) {
        self.canvas.borrow_mut().delete_image(self.id)
    }
}

#[derive(PartialEq, Eq, Hash, Debug)]
enum ImageCacheKey {
    #[cfg(not(target_arch = "wasm32"))]
    Path(String),
    EmbeddedData(by_address::ByAddress<&'static [u8]>),
}
// Cache used to avoid repeatedly decoding images from disk. The weak references are
// drained after flushing the renderer commands to the screen.
type ImageCacheRc = Rc<RefCell<HashMap<ImageCacheKey, Weak<CachedImage>>>>;

#[derive(Clone)]
enum GPUCachedData {
    Image(Rc<CachedImage>),
}

impl GPUCachedData {
    fn as_image(&self) -> &Rc<CachedImage> {
        match self {
            GPUCachedData::Image(image) => image,
            //_ => panic!("internal error. image requested for non-image gpu data"),
        }
    }
}

struct FontDatabase(HashMap<FontCacheKey, Rc<GLFont>>);

impl Default for FontDatabase {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

thread_local! {
    /// Database used to keep track of fonts added by the application
    pub static APPLICATION_FONTS: RefCell<fontdb::Database> = RefCell::new(fontdb::Database::new())
}

/// This function can be used to register a custom TrueType font with SixtyFPS,
/// for use with the `font-family` property. The provided slice must be a valid TrueType
/// font.
pub fn register_application_font_from_memory(
    data: &'static [u8],
) -> Result<(), Box<dyn std::error::Error>> {
    APPLICATION_FONTS.with(|fontdb| fontdb.borrow_mut().load_font_data(data.into()));
    Ok(())
}

fn try_load_app_font(canvas: &CanvasRc, request: &FontRequest) -> Option<GLFont> {
    let family = if request.family.is_empty() {
        fontdb::Family::SansSerif
    } else {
        fontdb::Family::Name(&request.family)
    };
    let query = fontdb::Query {
        families: &[family],
        weight: fontdb::Weight(request.weight as u16),
        ..Default::default()
    };
    APPLICATION_FONTS.with(|font_db| {
        let font_db = font_db.borrow();
        font_db.query(&query).and_then(|id| font_db.face_source(id)).map(|(source, _index)| {
            GLFont {
                // pass index to femtovg once femtovg/femtovg/pull/21 is merged
                font_id: match source.as_ref() {
                    fontdb::Source::Binary(data) => {
                        canvas.borrow_mut().add_font_mem(&data).unwrap()
                    }
                    fontdb::Source::File(path) => canvas.borrow_mut().add_font(path).unwrap(),
                },
                canvas: canvas.clone(),
            }
        })
    })
}

fn load_system_font(canvas: &CanvasRc, request: &FontRequest) -> GLFont {
    let family_name = if request.family.len() == 0 {
        font_kit::family_name::FamilyName::SansSerif
    } else {
        font_kit::family_name::FamilyName::Title(request.family.to_string())
    };

    let handle = font_kit::source::SystemSource::new()
        .select_best_match(
            &[family_name, font_kit::family_name::FamilyName::SansSerif],
            &font_kit::properties::Properties::new()
                .weight(font_kit::properties::Weight(request.weight as f32)),
        )
        .unwrap();

    // pass index to femtovg once femtovg/femtovg/pull/21 is merged
    let canvas_font = match handle {
        font_kit::handle::Handle::Path { path, font_index: _ } => {
            canvas.borrow_mut().add_font(path)
        }
        font_kit::handle::Handle::Memory { bytes, font_index: _ } => {
            canvas.borrow_mut().add_font_mem(bytes.as_slice())
        }
    }
    .unwrap();
    GLFont { font_id: canvas_font, canvas: canvas.clone() }
}

impl FontDatabase {
    fn font(&mut self, canvas: &CanvasRc, request: FontRequest) -> Rc<GLFont> {
        self.0
            .entry(FontCacheKey::new(&request))
            .or_insert_with(|| {
                Rc::new(
                    try_load_app_font(canvas, &request)
                        .unwrap_or_else(|| load_system_font(canvas, &request)),
                )
            })
            .clone()
    }
}

pub struct GLRenderer {
    canvas: CanvasRc,

    #[cfg(target_arch = "wasm32")]
    window: Rc<winit::window::Window>,
    #[cfg(target_arch = "wasm32")]
    event_loop_proxy: Rc<winit::event_loop::EventLoopProxy<eventloop::CustomEvent>>,
    #[cfg(not(target_arch = "wasm32"))]
    windowed_context: Option<glutin::WindowedContext<glutin::NotCurrent>>,

    item_rendering_cache: ItemRenderingCacheRc,
    image_cache: ImageCacheRc,

    loaded_fonts: Rc<RefCell<FontDatabase>>,
}

impl GLRenderer {
    pub fn new(
        event_loop: &winit::event_loop::EventLoop<eventloop::CustomEvent>,
        window_builder: winit::window::WindowBuilder,
        #[cfg(target_arch = "wasm32")] canvas_id: &str,
    ) -> GLRenderer {
        #[cfg(not(target_arch = "wasm32"))]
        let (windowed_context, renderer) = {
            let windowed_context = glutin::ContextBuilder::new()
                .with_vsync(true)
                .build_windowed(window_builder, &event_loop)
                .unwrap();
            let windowed_context = unsafe { windowed_context.make_current().unwrap() };

            let renderer = femtovg::renderer::OpenGl::new(|symbol| {
                windowed_context.get_proc_address(symbol) as *const _
            })
            .unwrap();

            #[cfg(target_os = "macos")]
            {
                use cocoa::appkit::NSView;
                use winit::platform::macos::WindowExtMacOS;
                let ns_view = windowed_context.window().ns_view();
                let view_id: cocoa::base::id = ns_view as *const _ as *mut _;
                unsafe {
                    NSView::setLayerContentsPlacement(view_id, cocoa::appkit::NSViewLayerContentsPlacement::NSViewLayerContentsPlacementTopLeft)
                }
            }

            (windowed_context, renderer)
        };

        #[cfg(target_arch = "wasm32")]
        let event_loop_proxy = Rc::new(event_loop.create_proxy());

        #[cfg(target_arch = "wasm32")]
        let (window, renderer) = {
            let canvas = web_sys::window()
                .unwrap()
                .document()
                .unwrap()
                .get_element_by_id(canvas_id)
                .unwrap()
                .dyn_into::<web_sys::HtmlCanvasElement>()
                .unwrap();

            use winit::platform::web::WindowBuilderExtWebSys;
            use winit::platform::web::WindowExtWebSys;

            let existing_canvas_size = winit::dpi::LogicalSize::new(
                canvas.client_width() as u32,
                canvas.client_height() as u32,
            );

            let window =
                Rc::new(window_builder.with_canvas(Some(canvas)).build(&event_loop).unwrap());

            // Try to maintain the existing size of the canvas element. A window created with winit
            // on the web will always have 1024x768 as size otherwise.

            let resize_canvas = {
                let event_loop_proxy = event_loop_proxy.clone();
                let canvas = web_sys::window()
                    .unwrap()
                    .document()
                    .unwrap()
                    .get_element_by_id(canvas_id)
                    .unwrap()
                    .dyn_into::<web_sys::HtmlCanvasElement>()
                    .unwrap();
                let window = window.clone();
                move |_: web_sys::Event| {
                    let existing_canvas_size = winit::dpi::LogicalSize::new(
                        canvas.client_width() as u32,
                        canvas.client_height() as u32,
                    );

                    window.set_inner_size(existing_canvas_size);
                    window.request_redraw();
                    event_loop_proxy.send_event(eventloop::CustomEvent::WakeUpAndPoll).ok();
                }
            };

            let resize_closure =
                wasm_bindgen::closure::Closure::wrap(Box::new(resize_canvas) as Box<dyn FnMut(_)>);
            web_sys::window()
                .unwrap()
                .add_event_listener_with_callback("resize", resize_closure.as_ref().unchecked_ref())
                .unwrap();
            resize_closure.forget();

            {
                let default_size = window.inner_size().to_logical(window.scale_factor());
                let new_size = winit::dpi::LogicalSize::new(
                    if existing_canvas_size.width > 0 {
                        existing_canvas_size.width
                    } else {
                        default_size.width
                    },
                    if existing_canvas_size.height > 0 {
                        existing_canvas_size.height
                    } else {
                        default_size.height
                    },
                );
                if new_size != default_size {
                    window.set_inner_size(new_size);
                }
            }

            let renderer = femtovg::renderer::OpenGl::new_from_html_canvas(window.canvas());
            (window, renderer)
        };

        let canvas = femtovg::Canvas::new(renderer).unwrap();

        GLRenderer {
            canvas: Rc::new(RefCell::new(canvas)),
            #[cfg(target_arch = "wasm32")]
            window,
            #[cfg(target_arch = "wasm32")]
            event_loop_proxy,
            #[cfg(not(target_arch = "wasm32"))]
            windowed_context: Some(unsafe { windowed_context.make_not_current().unwrap() }),
            item_rendering_cache: Default::default(),
            image_cache: Default::default(),
            loaded_fonts: Default::default(),
        }
    }
}

impl GraphicsBackend for GLRenderer {
    type ItemRenderer = GLItemRenderer;

    fn new_renderer(&mut self, clear_color: &Color) -> GLItemRenderer {
        let size = self.window().inner_size();

        #[cfg(not(target_arch = "wasm32"))]
        let current_windowed_context =
            unsafe { self.windowed_context.take().unwrap().make_current().unwrap() };

        {
            let mut canvas = self.canvas.borrow_mut();
            // We pass 1.0 as dpi / device pixel ratio as femtovg only uses this factor to scale
            // text metrics. Since we do the entire translation from logical pixels to physical
            // pixels on our end, we don't need femtovg to scale a second time.
            canvas.set_size(size.width, size.height, 1.0);
            canvas.clear_rect(0, 0, size.width, size.height, clear_color.into());
        }

        let scale_factor = current_windowed_context.window().scale_factor() as f32;
        GLItemRenderer {
            canvas: self.canvas.clone(),
            windowed_context: current_windowed_context,
            clip_rects: Default::default(),
            item_rendering_cache: self.item_rendering_cache.clone(),
            image_cache: self.image_cache.clone(),
            scale_factor,
            loaded_fonts: self.loaded_fonts.clone(),
        }
    }

    fn flush_renderer(&mut self, renderer: GLItemRenderer) {
        self.canvas.borrow_mut().flush();

        #[cfg(not(target_arch = "wasm32"))]
        {
            renderer.windowed_context.swap_buffers().unwrap();

            self.windowed_context =
                Some(unsafe { renderer.windowed_context.make_not_current().unwrap() });
        }

        self.image_cache.borrow_mut().retain(|_, cached_image_weak| {
            cached_image_weak
                .upgrade()
                .map_or(false, |cached_image_rc| Rc::strong_count(&cached_image_rc) > 1)
        });
    }

    fn release_item_graphics_cache(&self, data: &CachedRenderingData) {
        data.release(&mut self.item_rendering_cache.borrow_mut())
    }

    fn window(&self) -> &winit::window::Window {
        #[cfg(not(target_arch = "wasm32"))]
        return self.windowed_context.as_ref().unwrap().window();
        #[cfg(target_arch = "wasm32")]
        return &self.window;
    }

    fn font(&mut self, request: FontRequest) -> Rc<dyn Font> {
        self.loaded_fonts.borrow_mut().font(&self.canvas, request) as Rc<dyn Font>
    }
}

pub struct GLItemRenderer {
    canvas: CanvasRc,

    #[cfg(not(target_arch = "wasm32"))]
    windowed_context: glutin::WindowedContext<glutin::PossiblyCurrent>,

    clip_rects: SharedVector<Rect>,

    item_rendering_cache: ItemRenderingCacheRc,
    image_cache: ImageCacheRc,
    loaded_fonts: Rc<RefCell<FontDatabase>>,
    scale_factor: f32,
}

impl GLItemRenderer {
    // Look up the given image cache key in the image cache and upgrade the weak reference to a strong one if found,
    // otherwise a new image is created/loaded from the given callback.
    fn lookup_image_in_cache_or_create(
        &self,
        cache_key: ImageCacheKey,
        image_create_fn: impl Fn() -> femtovg::ImageId,
    ) -> Rc<CachedImage> {
        match self.image_cache.borrow_mut().entry(cache_key) {
            std::collections::hash_map::Entry::Occupied(mut existing_entry) => {
                existing_entry.get().upgrade().unwrap_or_else(|| {
                    let new_image =
                        Rc::new(CachedImage { id: image_create_fn(), canvas: self.canvas.clone() });
                    existing_entry.insert(Rc::downgrade(&new_image));
                    new_image
                })
            }
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                let new_image =
                    Rc::new(CachedImage { id: image_create_fn(), canvas: self.canvas.clone() });
                vacant_entry.insert(Rc::downgrade(&new_image));
                new_image
            }
        }
    }

    // Try to load the image the given resource points to
    fn load_image_resource(&self, resource: Resource) -> Option<GPUCachedData> {
        Some(GPUCachedData::Image(match resource {
            Resource::None => return None,
            Resource::AbsoluteFilePath(path) => {
                self.lookup_image_in_cache_or_create(ImageCacheKey::Path(path.to_string()), || {
                    self.canvas
                        .borrow_mut()
                        .load_image_file(
                            std::path::Path::new(&path.as_str()),
                            femtovg::ImageFlags::empty(),
                        )
                        .unwrap()
                })
            }
            Resource::EmbeddedData(data) => self.lookup_image_in_cache_or_create(
                ImageCacheKey::EmbeddedData(by_address::ByAddress(data.as_slice())),
                || {
                    self.canvas
                        .borrow_mut()
                        .load_image_mem(data.as_slice(), femtovg::ImageFlags::empty())
                        .unwrap()
                },
            ),
            Resource::EmbeddedRgbaImage { .. } => todo!(),
        }))
    }

    // Load the image from the specified Resource property (via getter fn), unless it was cached in the item's rendering
    // cache.
    fn load_cached_item_image(
        &self,
        item_cache: &CachedRenderingData,
        source_property_getter: impl Fn() -> Resource,
    ) -> Option<(Rc<CachedImage>, femtovg::ImageInfo)> {
        let mut cache = self.item_rendering_cache.borrow_mut();
        item_cache
            .ensure_up_to_date(&mut cache, || self.load_image_resource(source_property_getter()))
            .map(|gpu_resource| {
                let image = gpu_resource.as_image();
                (image.clone(), self.canvas.borrow().image_info(image.id).unwrap())
            })
    }
}

fn rect_to_path(r: Rect) -> femtovg::Path {
    let mut path = femtovg::Path::new();
    path.rect(r.min_x(), r.min_y(), r.width(), r.height());
    path
}

impl ItemRenderer for GLItemRenderer {
    fn draw_rectangle(
        &mut self,
        pos: Point,
        rect: std::pin::Pin<&sixtyfps_corelib::items::Rectangle>,
    ) {
        // TODO: cache path in item to avoid re-tesselation
        let mut path = rect_to_path(rect.geometry());
        let paint = femtovg::Paint::color(rect.color().into());
        self.canvas.borrow_mut().save_with(|canvas| {
            canvas.translate(pos.x, pos.y);
            canvas.fill_path(&mut path, paint)
        })
    }

    fn draw_border_rectangle(
        &mut self,
        pos: Point,
        rect: std::pin::Pin<&sixtyfps_corelib::items::BorderRectangle>,
    ) {
        // In CSS the border is entirely towards the inside of the boundary
        // geometry, while in femtovg the line with for a stroke is 50% in-
        // and 50% outwards. We choose the CSS model, so the inner rectangle
        // is adjusted accordingly.
        let border_width = rect.border_width();
        let mut path = femtovg::Path::new();
        path.rounded_rect(
            rect.x() + border_width / 2.,
            rect.y() + border_width / 2.,
            rect.width() - border_width,
            rect.height() - border_width,
            rect.border_radius(),
        );

        let fill_paint = femtovg::Paint::color(rect.color().into());

        let mut border_paint = femtovg::Paint::color(rect.border_color().into());
        border_paint.set_line_width(border_width);

        self.canvas.borrow_mut().save_with(|canvas| {
            canvas.translate(pos.x, pos.y);
            canvas.fill_path(&mut path, fill_paint);
            canvas.stroke_path(&mut path, border_paint);
        })
    }

    fn draw_image(&mut self, pos: Point, image: std::pin::Pin<&sixtyfps_corelib::items::Image>) {
        let (cached_image, image_info) =
            match self.load_cached_item_image(&image.cached_rendering_data, || image.source()) {
                Some(image) => image,
                None => return,
            };

        let image_id = cached_image.id;

        let (image_width, image_height) = (image_info.width() as f32, image_info.height() as f32);
        let (source_width, source_height) = (image_width, image_height);
        let fill_paint =
            femtovg::Paint::image(image_id, 0., 0., source_width, source_height, 0.0, 1.0);

        let mut path = femtovg::Path::new();
        path.rect(0., 0., image_width, image_height);

        self.canvas.borrow_mut().save_with(|canvas| {
            canvas.translate(pos.x + image.x(), pos.y + image.y());

            let scaled_width = image.width();
            let scaled_height = image.height();
            if scaled_width > 0. && scaled_height > 0. {
                canvas.scale(scaled_width / image_width, scaled_height / image_height);
            }

            canvas.fill_path(&mut path, fill_paint);
        })
    }

    fn draw_clipped_image(
        &mut self,
        pos: Point,
        clipped_image: std::pin::Pin<&sixtyfps_corelib::items::ClippedImage>,
    ) {
        let (cached_image, image_info) = match self
            .load_cached_item_image(&clipped_image.cached_rendering_data, || clipped_image.source())
        {
            Some(image) => image,
            None => return,
        };

        let source_clip_rect = Rect::new(
            [clipped_image.source_clip_x() as _, clipped_image.source_clip_y() as _].into(),
            [0., 0.].into(),
        );

        let (image_width, image_height) = (image_info.width() as f32, image_info.height() as f32);
        let (source_width, source_height) = if source_clip_rect.is_empty() {
            (image_width, image_height)
        } else {
            (source_clip_rect.width() as _, source_clip_rect.height() as _)
        };
        let fill_paint = femtovg::Paint::image(
            cached_image.id,
            source_clip_rect.min_x(),
            source_clip_rect.min_y(),
            source_width,
            source_height,
            0.0,
            1.0,
        );

        let mut path = femtovg::Path::new();
        path.rect(0., 0., image_width, image_height);

        self.canvas.borrow_mut().save_with(|canvas| {
            canvas.translate(pos.x + clipped_image.x(), pos.y + clipped_image.y());

            let scaled_width = clipped_image.width();
            let scaled_height = clipped_image.height();
            if scaled_width > 0. && scaled_height > 0. {
                canvas.scale(scaled_width / image_width, scaled_height / image_height);
            }

            canvas.fill_path(&mut path, fill_paint);
        })
    }

    fn draw_text(&mut self, pos: Point, text: std::pin::Pin<&sixtyfps_corelib::items::Text>) {
        use sixtyfps_corelib::items::{TextHorizontalAlignment, TextVerticalAlignment};

        let font = self.loaded_fonts.borrow_mut().font(&self.canvas, text.font_request());

        let mut paint = femtovg::Paint::color(text.color().into());
        paint.set_font(&[font.font_id]);
        paint.set_font_size(text.font_pixel_size(self.scale_factor()));
        paint.set_text_baseline(femtovg::Baseline::Top);

        let text_str = text.text();

        let max_width = text.width();
        let max_height = text.height();
        let (text_width, text_height) = {
            let text_metrics =
                self.canvas.borrow_mut().measure_text(0., 0., &text_str, paint).unwrap();
            let font_metrics = self.canvas.borrow_mut().measure_font(paint).unwrap();
            (text_metrics.width(), font_metrics.height())
        };

        let translate_x = match text.horizontal_alignment() {
            TextHorizontalAlignment::align_left => 0.,
            TextHorizontalAlignment::align_center => max_width / 2. - text_width / 2.,
            TextHorizontalAlignment::align_right => max_width - text_width,
        };

        let translate_y = match text.vertical_alignment() {
            TextVerticalAlignment::align_top => 0.,
            TextVerticalAlignment::align_center => max_height / 2. - text_height / 2.,
            TextVerticalAlignment::align_bottom => max_height - text_height,
        };

        self.canvas
            .borrow_mut()
            .fill_text(
                pos.x + text.x() + translate_x,
                pos.y + text.y() + translate_y,
                text_str,
                paint,
            )
            .unwrap();
    }

    fn draw_text_input(
        &mut self,
        _pos: Point,
        _rect: std::pin::Pin<&sixtyfps_corelib::items::TextInput>,
    ) {
        //todo!()
    }

    fn draw_path(&mut self, _pos: Point, _path: std::pin::Pin<&sixtyfps_corelib::items::Path>) {
        //todo!()
    }

    fn combine_clip(&mut self, pos: Point, clip: &std::pin::Pin<&sixtyfps_corelib::items::Clip>) {
        let clip_rect = clip.geometry().translate([pos.x, pos.y].into());
        self.canvas.borrow_mut().intersect_scissor(
            clip_rect.min_x(),
            clip_rect.min_y(),
            clip_rect.width(),
            clip_rect.height(),
        );
        self.clip_rects.push(clip_rect);
    }

    fn clip_rects(&self) -> SharedVector<sixtyfps_corelib::graphics::Rect> {
        self.clip_rects.clone()
    }

    fn reset_clip(&mut self, rects: SharedVector<sixtyfps_corelib::graphics::Rect>) {
        self.clip_rects = rects;
        // ### Only do this if rects were really changed
        let mut canvas = self.canvas.borrow_mut();
        canvas.reset_scissor();
        for rect in self.clip_rects.as_slice() {
            canvas.intersect_scissor(rect.min_x(), rect.min_y(), rect.width(), rect.height())
        }
    }

    fn draw_cached_pixmap(
        &mut self,
        item_cache: &CachedRenderingData,
        pos: Point,
        update_fn: &dyn Fn(&mut dyn FnMut(u32, u32, &[u8])),
    ) {
        let canvas = &self.canvas;
        let mut cache = self.item_rendering_cache.borrow_mut();

        let cached_image = item_cache.ensure_up_to_date(&mut cache, || {
            let mut cached_image = None;
            update_fn(&mut |width: u32, height: u32, data: &[u8]| {
                use rgb::FromSlice;
                let img = imgref::Img::new(data.as_rgba(), width as usize, height as usize);
                if let Some(image_id) =
                    canvas.borrow_mut().create_image(img, femtovg::ImageFlags::PREMULTIPLIED).ok()
                {
                    cached_image = Some(GPUCachedData::Image(Rc::new(CachedImage {
                        id: image_id,
                        canvas: canvas.clone(),
                    })))
                };
            });
            cached_image
        });
        let image_id = match cached_image {
            Some(x) => x.as_image().id,
            None => return,
        };
        let mut canvas = self.canvas.borrow_mut();

        let image_info = canvas.image_info(image_id).unwrap();
        let (width, height) = (image_info.width() as f32, image_info.height() as f32);
        let fill_paint = femtovg::Paint::image(image_id, pos.x, pos.y, width, height, 0.0, 1.0);
        let mut path = femtovg::Path::new();
        path.rect(pos.x, pos.y, width, height);
        canvas.fill_path(&mut path, fill_paint);
    }

    fn scale_factor(&self) -> f32 {
        self.scale_factor
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct FontCacheKey {
    family: SharedString,
    weight: i32,
}

impl FontCacheKey {
    fn new(request: &FontRequest) -> Self {
        Self { family: request.family.clone(), weight: request.weight }
    }
}

struct GLFont {
    font_id: femtovg::FontId,
    canvas: CanvasRc,
}

impl Font for GLFont {
    fn text_width(&self, pixel_size: f32, text: &str) -> f32 {
        let mut paint = femtovg::Paint::default();
        paint.set_font(&[self.font_id]);
        paint.set_font_size(pixel_size);
        self.canvas.borrow_mut().measure_text(0., 0., text, paint).unwrap().width()
    }

    fn text_offset_for_x_position<'a>(&self, _pixel_size: f32, _text: &'a str, _x: f32) -> usize {
        //todo!()
        return 0;
    }

    fn height(&self, pixel_size: f32) -> f32 {
        let mut paint = femtovg::Paint::default();
        paint.set_font(&[self.font_id]);
        paint.set_font_size(pixel_size);
        self.canvas.borrow_mut().measure_font(paint).unwrap().height()
    }
}

pub fn create_window() -> ComponentWindow {
    ComponentWindow::new(GraphicsWindow::new(|event_loop, window_builder| {
        GLRenderer::new(
            &event_loop.get_winit_event_loop(),
            window_builder,
            #[cfg(target_arch = "wasm32")]
            "canvas",
        )
    }))
}

#[cfg(target_arch = "wasm32")]
pub fn create_gl_window_with_canvas_id(canvas_id: String) -> ComponentWindow {
    ComponentWindow::new(GraphicsWindow::new(move |event_loop, window_builder| {
        GLRenderer::new(&event_loop.get_winit_event_loop(), window_builder, &canvas_id)
    }))
}

#[doc(hidden)]
#[cold]
pub fn use_modules() {
    sixtyfps_corelib::use_modules();
}

pub type NativeWidgets = ();
pub type NativeGlobals = ();
pub mod native_widgets {}
pub const HAS_NATIVE_STYLE: bool = false;
