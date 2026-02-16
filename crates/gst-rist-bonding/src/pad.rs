use crate::util::lock_or_recover;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::Mutex;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RsRistBondSinkPad {
        pub uri: Mutex<String>,
        pub interface: Mutex<Option<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RsRistBondSinkPad {
        const NAME: &'static str = "RsRistBondSinkPad";
        type Type = super::RsRistBondSinkPad;
        type ParentType = gst::Pad;
    }

    impl ObjectImpl for RsRistBondSinkPad {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::OnceLock<Vec<glib::ParamSpec>> =
                std::sync::OnceLock::new();
            PROPERTIES.get_or_init(|| {
                vec![
                    glib::ParamSpecString::builder("uri")
                        .nick("URI")
                        .blurb("RIST URI for this link (e.g. rist://1.2.3.4:5000)")
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
                            if let Ok(sink) = parent.downcast::<crate::sink::RsRistBondSink>() {
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

    impl GstObjectImpl for RsRistBondSinkPad {}
    impl PadImpl for RsRistBondSinkPad {}
}

glib::wrapper! {
    pub struct RsRistBondSinkPad(ObjectSubclass<imp::RsRistBondSinkPad>)
        @extends gst::Pad, gst::Object;
}

impl RsRistBondSinkPad {
    pub fn get_uri(&self) -> String {
        lock_or_recover(&self.imp().uri).clone()
    }

    pub fn get_interface(&self) -> Option<String> {
        lock_or_recover(&self.imp().interface).clone()
    }
}
