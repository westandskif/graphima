/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * Copyright (C) 2023, Nikita Almakov
 */
use crate::controls::{MouseControls, TouchControls, WatchControls};
use crate::events::JsEventListener;
use crate::main_chart::{DrawChart, MainChart};
use crate::params::{ChartConfig, ChartParams, ClientCaps};
use crate::scale::{LinearScale, LogScale, Scale};
use js_sys::Reflect;
use std::cell::RefCell;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

pub struct ChartManager {
    global_pointer_move: Option<JsEventListener>,
    global_pointer_out: Option<JsEventListener>,
    global_pointer_down: Option<JsEventListener>,
    global_pointer_up: Option<JsEventListener>,
    global_window_resize: Option<JsEventListener>,
    global_orintation_change: Option<JsEventListener>,
    global_request_animation_frame_closure: Option<Closure<dyn Fn(JsValue)>>,
    animation_frame_requested: bool,
    charts: Rc<RefCell<Vec<Box<dyn DrawChart>>>>,
    chart_ids: Vec<String>,
    control_watcher: Rc<RefCell<Box<dyn WatchControls>>>,
    touch_device: bool,
    client_caps: Rc<RefCell<ClientCaps>>,
    _pin: PhantomPinned,
}
impl ChartManager {
    // https://rustwasm.github.io/docs/wasm-bindgen/reference/attributes/on-rust-exports/constructor.html
    pub fn new() -> Pin<Box<Self>> {
        let touch_device = Self::is_touch_device();
        Box::pin(Self {
            global_pointer_move: None,
            global_pointer_out: None,
            global_pointer_up: None,
            global_pointer_down: None,
            global_window_resize: None,
            global_orintation_change: None,
            global_request_animation_frame_closure: None,
            animation_frame_requested: false,
            charts: Rc::new(RefCell::new(Vec::new())),
            chart_ids: Vec::new(),
            control_watcher: Rc::new(RefCell::new(if touch_device {
                Box::new(TouchControls::new())
            } else {
                Box::new(MouseControls::new())
            })),
            touch_device,
            client_caps: Rc::new(RefCell::new(ClientCaps::detect())),
            _pin: PhantomPinned,
        })
    }
    pub fn create_main(
        mut self: Pin<&mut Self>,
        raw_params: JsValue,
        raw_config: JsValue,
    ) -> Result<String, String> {
        let chart_config =
            ChartConfig::from_raw(&raw_config).map_err(|e| format!("config: {}", e.as_str()))?;
        let mut chart_params = ChartParams::from(&raw_params, &chart_config)
            .map_err(|e| format!("params: {}", e.as_str()))?;

        chart_params
            .content
            .sort_data_sets(&chart_config.sort_data_sets_by);

        let content_wrapper_selector =
            Self::inject_content_wrapper(chart_params.selector.as_str())?;
        unsafe { self.as_mut().get_unchecked_mut() }
            .chart_ids
            .push(content_wrapper_selector.clone());
        chart_params.selector = content_wrapper_selector.clone();

        let log_main_scale = LogScale::new(&chart_params.content);
        let linear_main_scale = LinearScale::new(&chart_params.content);
        let mut min_log_covered_square: f64 = f64::MAX;
        let mut min_linear_covered_square: f64 = f64::MAX;
        for data_set in chart_params.content.data_sets.iter() {
            let log_covered_square = log_main_scale.normalize_value(data_set.meta.max)
                - log_main_scale.normalize_value(data_set.meta.min);
            let linear_covered_square = linear_main_scale.normalize_value(data_set.meta.max)
                - linear_main_scale.normalize_value(data_set.meta.min);
            if log_covered_square != linear_covered_square {
                min_log_covered_square = min_log_covered_square.min(log_covered_square);
                min_linear_covered_square = min_linear_covered_square.min(linear_covered_square);
            }
        }

        if min_log_covered_square
            > min_linear_covered_square * chart_config.auto_log_scale_threshold
        {
            let preview_scale = LogScale::new(&chart_params.content);
            self.charts.borrow_mut().push(Box::new(MainChart::new(
                chart_params,
                chart_config,
                Rc::clone(&self.client_caps),
                log_main_scale,
                preview_scale,
            )?));
        } else {
            let preview_scale = LinearScale::new(&chart_params.content);
            self.charts.borrow_mut().push(Box::new(MainChart::new(
                chart_params,
                chart_config,
                Rc::clone(&self.client_caps),
                linear_main_scale,
                preview_scale,
            )?));
        };

        unsafe { self.as_mut().get_unchecked_mut() }.ensure_global_listeners_are_set_up();
        Ok(content_wrapper_selector)
    }

    pub fn destroy_main(mut self: Pin<&mut Self>, chart_id: JsValue) -> Result<(), String> {
        let chart_id = chart_id
            .as_string()
            .ok_or_else(|| "not a string".to_string())?;
        let index = self
            .chart_ids
            .iter()
            .position(|id| id == chart_id.as_str())
            .ok_or_else(|| "chart not found by id".to_string())?;
        let document = web_sys::window().unwrap().document().unwrap();
        let chart_wrapper = document
            .query_selector(chart_id.as_str())
            .unwrap()
            .ok_or_else(|| "chart wrapper not found in dom".to_string())?;
        chart_wrapper.remove();

        let chart_manager = unsafe { self.as_mut().get_unchecked_mut() };
        chart_manager.chart_ids.remove(index);
        let charts = &mut chart_manager.charts;
        charts.borrow_mut().remove(index);
        if charts.borrow().len() == 0 {
            unsafe { self.as_mut().get_unchecked_mut() }.uninstall_listeners();
        }
        Ok(())
    }

    fn uninstall_listeners(&mut self) {
        self.global_pointer_move = None;
        self.global_pointer_out = None;
        self.global_pointer_down = None;
        self.global_pointer_up = None;
        self.global_window_resize = None;
        self.global_orintation_change = None;
    }

    fn ensure_global_listeners_are_set_up(&mut self) {
        if self.global_pointer_move.is_some() {
            return;
        }
        let charts = Rc::clone(&self.charts);
        let control_watcher = Rc::clone(&self.control_watcher);
        let ptr = self as *mut Self;
        self.global_pointer_down = Some(JsEventListener::new(
            web_sys::window().unwrap().into(),
            if self.touch_device {
                "touchstart"
            } else {
                "mousedown"
            },
            Box::new(move |event: JsValue| {
                if let Some(control_event) = control_watcher.borrow_mut().down(&event) {
                    let time_us = Self::get_time_us();
                    for chart in charts.borrow_mut().iter_mut() {
                        chart.on_control_event(&control_event, time_us);
                    }
                    unsafe { ptr.as_mut().unwrap().request_animation_frame() }
                }
            }),
        ));
        let charts = Rc::clone(&self.charts);
        let control_watcher = Rc::clone(&self.control_watcher);
        self.global_pointer_up = Some(JsEventListener::new(
            web_sys::window().unwrap().into(),
            if self.touch_device {
                "touchend"
            } else {
                "mouseup"
            },
            Box::new(move |event: JsValue| {
                if let Some(control_event) = control_watcher.borrow_mut().up(&event) {
                    let time_us = Self::get_time_us();
                    for chart in charts.borrow_mut().iter_mut() {
                        chart.on_control_event(&control_event, time_us);
                    }
                    unsafe { ptr.as_mut().unwrap().request_animation_frame() }
                }
            }),
        ));
        let charts = Rc::clone(&self.charts);
        let control_watcher = Rc::clone(&self.control_watcher);
        self.global_pointer_move = Some(JsEventListener::new(
            web_sys::window().unwrap().into(),
            if self.touch_device {
                "touchmove"
            } else {
                "mousemove"
            },
            Box::new(move |event: JsValue| {
                if let Some(control_event) = control_watcher.borrow_mut().moved(&event) {
                    let time_us = Self::get_time_us();
                    for chart in charts.borrow_mut().iter_mut() {
                        chart.on_control_event(&control_event, time_us);
                    }
                    unsafe { ptr.as_mut().unwrap().request_animation_frame() }
                }
            }),
        ));
        if self.touch_device {
            let charts = Rc::clone(&self.charts);
            let control_watcher = Rc::clone(&self.control_watcher);
            self.global_pointer_out = Some(JsEventListener::new(
                web_sys::window().unwrap().into(),
                "touchcancel",
                Box::new(move |event: JsValue| {
                    if let Some(control_event) = control_watcher.borrow_mut().left(&event) {
                        let time_us = Self::get_time_us();
                        for chart in charts.borrow_mut().iter_mut() {
                            chart.on_control_event(&control_event, time_us);
                        }
                        unsafe { ptr.as_mut().unwrap().request_animation_frame() }
                    }
                }),
            ));
        }
        let charts = Rc::clone(&self.charts);
        self.global_window_resize = Some(JsEventListener::new(
            web_sys::window().unwrap().into(),
            "resize",
            Box::new(move |_: JsValue| {
                for chart in charts.borrow_mut().iter_mut() {
                    chart.on_resize();
                }
                unsafe { ptr.as_mut().unwrap().request_animation_frame() }
            }),
        ));
        let client_caps = Rc::clone(&self.client_caps);
        let charts = Rc::clone(&self.charts);
        let ptr = self as *mut Self;
        if self.client_caps.borrow().screen_orientation {
            self.global_orintation_change = Some(JsEventListener::new(
                Reflect::get(&web_sys::window().unwrap(), &JsValue::from_str("screen"))
                    .and_then(|screen| Reflect::get(&screen, &JsValue::from_str("orientation")))
                    .unwrap()
                    .into(),
                "change",
                Box::new(move |_: JsValue| {
                    *client_caps.borrow_mut() = ClientCaps::detect();
                    for chart in charts.borrow_mut().iter_mut() {
                        chart.on_resize();
                    }
                    unsafe { ptr.as_mut().unwrap().request_animation_frame() }
                }),
            ));
        } else {
            self.global_orintation_change = Some(JsEventListener::new(
                web_sys::window().unwrap().into(),
                "orientationchange",
                Box::new(move |_: JsValue| {
                    *client_caps.borrow_mut() = ClientCaps::detect();
                    for chart in charts.borrow_mut().iter_mut() {
                        chart.on_resize();
                    }
                    unsafe { ptr.as_mut().unwrap().request_animation_frame() }
                }),
            ));
        }

        if self.global_request_animation_frame_closure.is_none() {
            let charts = Rc::clone(&self.charts);
            let ptr = self as *mut Self;
            let closure = Closure::new(Box::new(move |time_ms: JsValue| {
                unsafe { ptr.as_mut().unwrap().animation_frame_requested = false }

                let mut actions: usize = 0;
                let time_us = time_ms.as_f64().unwrap() * 1000.0;
                for chart in charts.borrow_mut().iter_mut() {
                    actions += chart.draw(time_us);
                }
                if actions > 0 {
                    unsafe { ptr.as_mut().unwrap().request_animation_frame() };
                }
            }));
            self.global_request_animation_frame_closure = Some(closure);
        }
        self.request_animation_frame();
    }
    fn request_animation_frame(&mut self) {
        if !self.animation_frame_requested {
            web_sys::window()
                .unwrap()
                .request_animation_frame(
                    self.global_request_animation_frame_closure
                        .as_ref()
                        .unwrap()
                        .as_ref()
                        .unchecked_ref(),
                )
                .unwrap();
            self.animation_frame_requested = true;
        }
    }
    fn inject_content_wrapper(selector: &str) -> Result<String, String> {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document
            .query_selector(selector)
            .unwrap()
            .ok_or_else(|| "container not found".to_string())?;

        let wrapper = document.create_element("div").unwrap();
        let content_wrapper_selector = format!(
            "ac-{}",
            (js_sys::Math::random() * 1000000.0).floor() as usize
        );
        container.append_child(&wrapper).unwrap();
        wrapper
            .set_attribute("id", content_wrapper_selector.as_str())
            .unwrap();
        wrapper
            .set_attribute("style", "width: 100%; height: 100%; position: relative")
            .unwrap();
        Ok(format!("#{}", content_wrapper_selector.as_str()))
    }
    fn is_touch_device() -> bool {
        let window = web_sys::window().unwrap();
        !Reflect::get(&window, &JsValue::from_str("ontouchstart"))
            .unwrap()
            .is_undefined()
            && window.navigator().max_touch_points() > 0
    }
    fn get_time_us() -> f64 {
        web_sys::window().unwrap().performance().unwrap().now() * 1000.0
    }
}

static mut CHART_MANAGER: Option<u32> = None;

pub fn get_or_create_manager_addr() -> u32 {
    unsafe {
        match CHART_MANAGER {
            Some(addr) => addr,
            None => {
                let addr = Box::into_raw(Pin::into_inner_unchecked(ChartManager::new())) as u32;
                CHART_MANAGER = Some(addr);
                addr
            }
        }
    }
}
