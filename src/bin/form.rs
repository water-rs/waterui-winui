//! Form example for `waterui-winui`: a faithful port of the `waterui` `form`
//! example's scene onto a real `WinUI` window. The window stays open until it
//! is closed externally, so CI can screenshot the live application.
//!
//! Run on Windows with `cargo run --bin form`.

fn main() {
    #[cfg(target_os = "windows")]
    run();
    #[cfg(not(target_os = "windows"))]
    panic!("waterui-winui only runs on Windows");
}

#[cfg(target_os = "windows")]
fn run() {
    use waterui::env::Environment;

    waterui_winui::run_app(|| app(Environment::new())).expect("WinUI form run failed");
}

#[cfg(target_os = "windows")]
mod example {
    use waterui::app::App;
    use waterui::prelude::slider::slider;
    use waterui::prelude::stepper::stepper;
    use waterui::prelude::*;
    use waterui::reactive::binding;
    use waterui::text::font::{FontWeight, ResolvedFont};
    use waterui::window::{Window, WindowState};

    #[form]
    struct RegistrationForm {
        /// Full name of the user
        full_name: String,
        /// Email address for account
        email: String,
        /// Age in years
        age: i32,
        /// Whether to receive marketing emails
        newsletter: bool,
        /// Preferred volume level (0.0 - 1.0)
        volume: f64,
    }

    #[form]
    struct AppSettings {
        /// Application theme brightness
        brightness: f64,
        /// Enable dark mode
        dark_mode: bool,
        /// Font size multiplier
        font_scale: f32,
        /// Auto-save interval (minutes)
        auto_save_minutes: i32,
        /// Enable notifications
        notifications_enabled: bool,
    }

    #[allow(clippy::needless_pass_by_value)]
    fn scene(
        settings: Binding<AppSettings>,
        registration: Binding<RegistrationForm>,
        custom_name: Binding<Str>,
        custom_enabled: Binding<bool>,
        custom_count: Binding<i32>,
        custom_slider: Binding<f64>,
    ) -> impl View {
        scroll(
            vstack((
                text("WaterUI Form Examples").title(),
                "Demonstrating form building with reactive data binding",
                Divider,
                spacer(),
                vstack((
                    text("Registration Form").sub_headline(),
                    "Using #[form] derive macro",
                    form(&registration),
                    Divider,
                    text("Live Preview:").bold(),
                    text!(
                        "Name: {registration}",
                        registration = registration.project().full_name
                    ),
                    text!("Email: {email}", email = registration.project().email),
                    text!("Age: {age}", age = registration.project().age),
                    text!(
                        "Newsletter: {newsletter}",
                        newsletter = registration.project().newsletter
                    ),
                    text!("Volume: {volume}", volume = registration.project().volume),
                )),
                spacer(),
                vstack((
                    text("App Settings").sub_headline(),
                    "Another form with different field types",
                    form(&settings),
                    Divider,
                    text("Current Settings:").bold(),
                    hstack((
                        "Dark Mode: ",
                        text!("{dark_mode}", dark_mode = settings.project().dark_mode),
                    )),
                    hstack((
                        "Brightness: ",
                        text!(
                            "{brightness:.4}",
                            brightness = settings.project().brightness
                        ),
                    )),
                )),
                spacer(),
                vstack((
                    text("Manual Form Controls").sub_headline(),
                    "Building forms manually with individual controls",
                    TextField::new("Username", &custom_name).prompt("Enter your username"),
                    Toggle::new("Enable Feature", &custom_enabled),
                    stepper("Item Count", &custom_count).range(0..=100).step(5),
                    slider("Progress", &custom_slider),
                    progress(custom_slider.clone()),
                    Divider,
                    text("Manual Controls Preview:").bold(),
                    text!("Username: {custom_name}"),
                    text!("Feature Enabled: {custom_enabled}"),
                    text!("Count: {custom_count}"),
                    text!("Progress: {custom_slider}"),
                )),
                spacer(),
                Divider,
                "Built with WaterUI Form Components",
            ))
            .padding(),
        )
    }

    pub fn app(mut env: Environment) -> App {
        let settings = AppSettings::binding();
        let registration = RegistrationForm::binding();
        let custom_name = binding("");
        let custom_enabled = binding(false);
        let custom_count = binding(5);
        let custom_slider = binding(0.5);

        let theme = Theme::new()
            .color_scheme(
                settings
                    .project()
                    .dark_mode
                    .select(ColorScheme::Dark, ColorScheme::Light),
            )
            .fonts(FontSettings::new().body(
                settings.project().font_scale.map(|scale| {
                    ResolvedFont::new(16.0 + (1.0 + scale * 10.0), FontWeight::Normal)
                }),
            ));

        env.install(theme);

        App::new_with_windows(
            [Window::new(
                "WaterUI Form Example",
                binding(WindowState::Normal),
                move || {
                    scene(
                        settings.clone(),
                        registration.clone(),
                        custom_name.clone(),
                        custom_enabled.clone(),
                        custom_count.clone(),
                        custom_slider.clone(),
                    )
                },
            )],
            env,
        )
    }
}

#[cfg(target_os = "windows")]
use example::app;
