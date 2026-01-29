use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RsRistBondSinkPad {
        pub uri: std::sync::Mutex<String>,
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
                vec![glib::ParamSpecString::builder("uri")
                    .nick("URI")
                    .blurb("RIST URI for this link (e.g. rist://1.2.3.4:5000)")
                    .mutable_ready()
                    .build()]
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "uri" => {
                    let mut uri = self.uri.lock().unwrap();
                    *uri = value.get().expect("type checked upstream");
                    // Note: We might want to notify the parent sink to update the link.
                    // But usually, configuration happens before requesting/linking or dynamically.
                    // If changed dynamically, we need a way to notify the Sink logic to update the wrapper Link.
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "uri" => {
                    let uri = self.uri.lock().unwrap();
                    uri.to_value()
                }
                _ => unimplemented!(),
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
        self.imp().uri.lock().unwrap().clone()
    }
}
