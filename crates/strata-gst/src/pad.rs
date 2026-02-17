use crate::util::lock_or_recover;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::Mutex;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct StrataSinkPad {
        pub uri: Mutex<String>,
        pub interface: Mutex<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for StrataSinkPad {
        const NAME: &'static str = "StrataSinkPad";
        type Type = super::StrataSinkPad;
        type ParentType = gst::Pad;
    }

    impl ObjectImpl for StrataSinkPad {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                std::sync::OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("uri")
                        .nick("URI")
                        .blurb("Destination address for this link (host:port)")
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("interface")
                        .nick("Interface")
                        .blurb("OS network interface name (e.g. eth0) to bind this link to")
                        .mutable_ready()
                        .build(),
                ]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "uri" => {
                    let new_uri: String = value.get().expect("type checked upstream");
                    let mut uri = lock_or_recover(&self.uri);
                    let should_notify = *uri != new_uri;
                    *uri = new_uri;
                    drop(uri);
                    if should_notify {
                        if let Some(parent) = self.obj().parent() {
                            if let Ok(sink) = parent.downcast::<crate::sink::StrataSink>() {
                                sink.imp().add_link_from_pad(&self.obj());
                            }
                        }
                        self.obj().notify("uri");
                    }
                }
                "interface" => {
                    let iface: String = value.get().expect("type checked upstream");
                    let mut current = lock_or_recover(&self.interface);
                    *current = if iface.is_empty() { None } else { Some(iface) };
                }
                _ => {
                    gst::warning!(gst::CAT_DEFAULT, "Unknown pad property: {}", pspec.name());
                }
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "uri" => {
                    let uri = lock_or_recover(&self.uri);
                    uri.to_value()
                }
                "interface" => {
                    let iface = lock_or_recover(&self.interface);
                    iface.clone().unwrap_or_default().to_value()
                }
                _ => {
                    gst::warning!(gst::CAT_DEFAULT, "Unknown pad property: {}", pspec.name());
                    "".to_value()
                }
            }
        }
    }

    impl GstObjectImpl for StrataSinkPad {}
    impl PadImpl for StrataSinkPad {}
}

glib::wrapper! {
    pub struct StrataSinkPad(ObjectSubclass<imp::StrataSinkPad>)
        @extends gst::Pad, gst::Object;
}

impl StrataSinkPad {
    pub fn get_uri(&self) -> String {
        lock_or_recover(&self.imp().uri).clone()
    }

    pub fn get_interface(&self) -> Option<String> {
        lock_or_recover(&self.imp().interface).clone()
    }
}
