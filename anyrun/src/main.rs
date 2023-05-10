use std::{
    cell::RefCell,
    env, fs,
    io::{self, Write},
    mem,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

use abi_stable::std_types::{ROption, RVec};
use anyrun_interface::{HandleResult, Match, PluginRef, PollResult};
use gtk::{gdk, gdk_pixbuf, glib, prelude::*};
use nix::unistd;
use serde::Deserialize;
use wl_clipboard_rs::copy;

#[derive(Deserialize)]
struct Config {
    width: RelativeNum,
    vertical_offset: RelativeNum,
    position: Position,
    plugins: Vec<PathBuf>,
    hide_icons: bool,
    ignore_exclusive_zones: bool,
    close_on_click: bool,
    max_entries: Option<usize>,
    layer: Layer,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: RelativeNum::Absolute(800),
            vertical_offset: RelativeNum::Absolute(0),
            position: Position::Top,
            plugins: vec![
                "libapplications.so".into(),
                "libsymbols.so".into(),
                "libshell.so".into(),
                "libtranslate.so".into(),
            ],
            hide_icons: false,
            ignore_exclusive_zones: false,
            close_on_click: false,
            max_entries: None,
            layer: Layer::Overlay,
        }
    }
}

#[derive(Deserialize)]
enum Layer {
    Background,
    Bottom,
    Top,
    Overlay,
}

// Could have a better name
#[derive(Deserialize)]
enum RelativeNum {
    Absolute(i32),
    Fraction(f32),
}

/// A "view" of plugin's info and matches
#[derive(Clone)]
struct PluginView {
    plugin: PluginRef,
    row: gtk::ListBoxRow,
    list: gtk::ListBox,
}

struct Args {
    override_plugins: Option<Vec<String>>,
}

#[derive(Deserialize)]
enum Position {
    Top,
    Center,
}

/// Actions to run after GTK has finished
enum PostRunAction {
    Copy(Vec<u8>),
    None,
}

/// Some data that needs to be shared between various parts
struct RuntimeData {
    args: Args,
    /// A plugin may request exclusivity which is set with this
    exclusive: Option<PluginView>,
    plugins: Vec<PluginView>,
    post_run_action: PostRunAction,
    config: Config,
    /// Used for displaying errors later on
    error_label: String,
    config_dir: String,
}

/// The naming scheme for CSS styling
///
/// Refer to [GTK 3.0 CSS Overview](https://docs.gtk.org/gtk3/css-overview.html)
/// and [GTK 3.0 CSS Properties](https://docs.gtk.org/gtk3/css-properties.html) for how to style.
mod style_names {
    /// The text entry box
    pub const ENTRY: &str = "entry";
    /// "Main" widgets (main GtkListBox, main GtkBox)
    pub const MAIN: &str = "main";
    /// The window
    pub const WINDOW: &str = "window";
    /// Widgets related to the whole plugin. Including the info box
    pub const PLUGIN: &str = "plugin";
    /// Widgets for the specific match `MATCH_*` names are for more specific parts.
    pub const MATCH: &str = "match";

    pub const MATCH_TITLE: &str = "match-title";
    pub const MATCH_DESC: &str = "match-desc";
}

/// Default config directory
pub const DEFAULT_CONFIG_DIR: &str = "/etc/anyrun";

fn main() {
    let app = gtk::Application::new(Some("com.kirottu.anyrun"), Default::default());
    let runtime_data: Rc<RefCell<Option<RuntimeData>>> = Rc::new(RefCell::new(None));

    // Add the launch options to the GTK Application
    app.add_main_option(
        "override-plugins",
        glib::Char('o' as i8),
        glib::OptionFlags::IN_MAIN,
        glib::OptionArg::StringArray,
        "Override plugins. Provide paths in same format as in the config file",
        None,
    );
    app.add_main_option(
        "config-dir",
        glib::Char('c' as i8),
        glib::OptionFlags::IN_MAIN,
        glib::OptionArg::String,
        "Override the config directory from the default (~/.config/anyrun/)",
        None,
    );

    let runtime_data_clone = runtime_data.clone();
    app.connect_handle_local_options(move |_app, dict| {
        let override_plugins = dict.lookup::<Vec<String>>("override-plugins").unwrap();
        let config_dir = dict.lookup::<String>("config-dir").unwrap();

        // Figure out the config dir
        let user_dir = format!(
            "{}/.config/anyrun",
            env::var("HOME").expect("Could not determine home directory! Is $HOME set?")
        );
        let config_dir = config_dir.unwrap_or_else(|| {
            if PathBuf::from(&user_dir).exists() {
                user_dir
            } else {
                DEFAULT_CONFIG_DIR.to_string()
            }
        });

        // Load config, if unable to then read default config. If an error occurs the message will be displayed.
        let (config, error_label) = match fs::read_to_string(format!("{}/config.ron", config_dir)) {
            Ok(content) => ron::from_str(&content)
                .map(|config| (config, String::new()))
                .unwrap_or_else(|why| {
                    (
                        Config::default(),
                        format!(
                            "Failed to parse Anyrun config file, using default config: {}",
                            why
                        ),
                    )
                }),
            Err(why) => (
                Config::default(),
                format!(
                    "Failed to read Anyrun config file, using default config: {}",
                    why
                ),
            ),
        };

        *runtime_data_clone.borrow_mut() = Some(RuntimeData {
            args: Args { override_plugins },
            exclusive: None,
            plugins: Vec::new(),
            post_run_action: PostRunAction::None,
            error_label,
            config,
            config_dir,
        });
        -1 // Magic GTK number to continue running
    });

    let runtime_data_clone = runtime_data.clone();
    app.connect_activate(move |app| activate(app, runtime_data_clone.clone()));

    app.run();

    let runtime_data = runtime_data.borrow_mut().take().unwrap();

    // Perform a post run action if one is set
    match runtime_data.post_run_action {
        PostRunAction::Copy(bytes) => match unsafe { unistd::fork() } {
            // The parent process just exits and prints that out
            Ok(unistd::ForkResult::Parent { .. }) => {
                println!("Child spawned to serve copy requests.");
            }
            // Child process starts serving copy requests
            Ok(unistd::ForkResult::Child) => {
                let mut opts = copy::Options::new();
                opts.foreground(true);
                opts.copy(
                    copy::Source::Bytes(bytes.into_boxed_slice()),
                    copy::MimeType::Autodetect,
                )
                .expect("Failed to serve copy bytes");
            }
            Err(why) => {
                eprintln!("Failed to fork for copy sharing: {}", why);
            }
        },
        PostRunAction::None => (),
    }
}

fn activate(app: &gtk::Application, runtime_data: Rc<RefCell<Option<RuntimeData>>>) {
    // Create the main window
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .name(style_names::WINDOW)
        .build();

    // Init GTK layer shell
    gtk_layer_shell::init_for_window(&window);

    // Make layer-window fullscreen
    gtk_layer_shell::set_anchor(&window, gtk_layer_shell::Edge::Top, true);
    gtk_layer_shell::set_anchor(&window, gtk_layer_shell::Edge::Bottom, true);
    gtk_layer_shell::set_anchor(&window, gtk_layer_shell::Edge::Left, true);
    gtk_layer_shell::set_anchor(&window, gtk_layer_shell::Edge::Right, true);

    gtk_layer_shell::set_namespace(&window, "anyrun");

    if runtime_data
        .borrow()
        .as_ref()
        .unwrap()
        .config
        .ignore_exclusive_zones
    {
        gtk_layer_shell::set_exclusive_zone(&window, -1);
    }

    gtk_layer_shell::set_keyboard_mode(&window, gtk_layer_shell::KeyboardMode::Exclusive);

    match runtime_data.borrow().as_ref().unwrap().config.layer {
        Layer::Background => {
            gtk_layer_shell::set_layer(&window, gtk_layer_shell::Layer::Background)
        }
        Layer::Bottom => gtk_layer_shell::set_layer(&window, gtk_layer_shell::Layer::Bottom),
        Layer::Top => gtk_layer_shell::set_layer(&window, gtk_layer_shell::Layer::Top),
        Layer::Overlay => gtk_layer_shell::set_layer(&window, gtk_layer_shell::Layer::Overlay),
    };

    // Try to load custom CSS, if it fails load the default CSS
    let provider = gtk::CssProvider::new();
    if let Err(why) = provider.load_from_path(&format!(
        "{}/style.css",
        runtime_data.borrow().as_ref().unwrap().config_dir
    )) {
        eprintln!("Failed to load custom CSS: {}", why);
        provider
            .load_from_data(include_bytes!("../res/style.css"))
            .unwrap();
    }
    gtk::StyleContext::add_provider_for_screen(
        &gdk::Screen::default().expect("Failed to get GDK screen for CSS provider!"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Use the plugins in the config file, or the plugins specified with the override
    let plugins = match &runtime_data
        .borrow()
        .as_ref()
        .unwrap()
        .args
        .override_plugins
    {
        Some(plugins) => plugins.iter().map(PathBuf::from).collect(),
        None => runtime_data
            .borrow()
            .as_ref()
            .unwrap()
            .config
            .plugins
            .clone(),
    };

    // Make sure at least one plugin is specified
    if plugins.is_empty() {
        eprintln!("At least one plugin needs to be enabled!");
        app.quit();
    }

    // Create the main list of plugin views
    let main_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .name(style_names::MAIN)
        .build();

    // Prioritise the ANYRUN_PLUGINS env var over other paths
    let mut plugin_paths = match env::var("ANYRUN_PLUGINS") {
        Ok(string) => string.split(':').map(PathBuf::from).collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };

    plugin_paths.append(&mut vec![
        format!(
            "{}/plugins",
            runtime_data.borrow().as_ref().unwrap().config_dir
        )
        .into(),
        format!("{}/plugins", DEFAULT_CONFIG_DIR).into(),
    ]);

    // Load plugins from the paths specified in the config file
    runtime_data.borrow_mut().as_mut().unwrap().plugins = plugins
        .iter()
        .map(|plugin_path| {
            // Load the plugin's dynamic library.
            let mut user_path = PathBuf::from(&format!(
                "{}/plugins",
                runtime_data.borrow().as_ref().unwrap().config_dir
            ));
            let mut global_path = PathBuf::from("/etc/anyrun/plugins");
            user_path.extend(plugin_path.iter());
            global_path.extend(plugin_path.iter());

            // Load the plugin's dynamic library.

            let plugin = if plugin_path.is_absolute() {
                abi_stable::library::lib_header_from_path(plugin_path)
            } else {
                let path = plugin_paths
                    .clone()
                    .into_iter()
                    .map(|mut path| {
                        path.push(plugin_path);
                        path
                    })
                    .find(|path| path.exists())
                    .expect("Invalid plugin path");

                abi_stable::library::lib_header_from_path(&path)
            }
            .and_then(|plugin| plugin.init_root_module::<PluginRef>())
            .expect("Failed to load plugin");

            // Run the plugin's init code to init static resources etc.
            plugin.init()(
                runtime_data
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .config_dir
                    .clone()
                    .into(),
            );

            let plugin_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .name(style_names::PLUGIN)
                .build();
            plugin_box.add(
                &gtk::Label::builder()
                    .name(style_names::PLUGIN)
                    .label(&plugin.info()().name)
                    .halign(gtk::Align::Start)
                    .build(),
            );
            let list = gtk::ListBox::builder()
                .name(style_names::PLUGIN)
                .hexpand(true)
                .build();

            plugin_box.add(&list);

            let row = gtk::ListBoxRow::builder().name(style_names::PLUGIN).build();
            row.add(&plugin_box);

            main_list.add(&row);

            PluginView { plugin, row, list }
        })
        .collect::<Vec<PluginView>>();

    // Connect selection events to avoid completely messing up selection logic
    for plugin_view in runtime_data.borrow().as_ref().unwrap().plugins.iter() {
        let plugins_clone = runtime_data.borrow().as_ref().unwrap().plugins.clone();
        plugin_view.list.connect_row_selected(move |list, row| {
            if row.is_some() {
                let combined_matches = plugins_clone
                    .iter()
                    .flat_map(|view| {
                        view.list.children().into_iter().map(|child| {
                            (
                                child.dynamic_cast::<gtk::ListBoxRow>().unwrap(),
                                view.list.clone(),
                            )
                        })
                    })
                    .collect::<Vec<(gtk::ListBoxRow, gtk::ListBox)>>();

                // Unselect everything except the new selection
                for (_, _list) in combined_matches {
                    if _list != *list {
                        _list.select_row(None::<&gtk::ListBoxRow>);
                    }
                }
            }
        });
    }

    // Text entry box
    let entry = gtk::Entry::builder()
        .hexpand(true)
        .name(style_names::ENTRY)
        .build();

    // Refresh the matches when text input changes
    let runtime_data_clone = runtime_data.clone();
    entry.connect_changed(move |entry| {
        refresh_matches(entry.text().to_string(), runtime_data_clone.clone())
    });

    // Handle other key presses for selection control and all other things that may be needed
    let entry_clone = entry.clone();
    let runtime_data_clone = runtime_data.clone();

    window.connect_key_press_event(move |window, event| {
        use gdk::keys::constants;
        match event.keyval() {
            // Close window on escape
            constants::Escape => {
                window.close();
                Inhibit(true)
            }
            // Handle selections
            constants::Down | constants::Tab | constants::Up => {
                // Combine all of the matches into a `Vec` to allow for easier handling of the selection
                let combined_matches = runtime_data_clone
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .plugins
                    .iter()
                    .flat_map(|view| {
                        view.list.children().into_iter().map(|child| {
                            (
                                // All children of lists are GtkListBoxRow widgets
                                child.dynamic_cast::<gtk::ListBoxRow>().unwrap(),
                                view.list.clone(),
                            )
                        })
                    })
                    .collect::<Vec<(gtk::ListBoxRow, gtk::ListBox)>>();

                // Get the selected match
                let (selected_match, selected_list) = match runtime_data_clone
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .plugins
                    .iter()
                    .find_map(|view| view.list.selected_row().map(|row| (row, view.list.clone())))
                {
                    Some(selected) => selected,
                    None => {
                        // If nothing is selected select either the top or bottom match based on the input
                        if !combined_matches.is_empty() {
                            match event.keyval() {
                                constants::Down | constants::Tab => combined_matches[0]
                                    .1
                                    .select_row(Some(&combined_matches[0].0)),
                                constants::Up => combined_matches[combined_matches.len() - 1]
                                    .1
                                    .select_row(Some(
                                        &combined_matches[combined_matches.len() - 1].0,
                                    )),
                                _ => unreachable!(),
                            }
                        }
                        return Inhibit(true);
                    }
                };

                // Clear the previous selection
                selected_list.select_row(None::<&gtk::ListBoxRow>);

                // Get the index of the current selection
                let index = combined_matches
                    .iter()
                    .position(|(row, _)| *row == selected_match)
                    .unwrap();

                // Move the selection based on the input, loops from top to bottom and vice versa
                match event.keyval() {
                    constants::Down | constants::Tab => {
                        if index < combined_matches.len() - 1 {
                            combined_matches[index + 1]
                                .1
                                .select_row(Some(&combined_matches[index + 1].0));
                        } else {
                            combined_matches[0]
                                .1
                                .select_row(Some(&combined_matches[0].0));
                        }
                    }
                    constants::Up => {
                        if index > 0 {
                            combined_matches[index - 1]
                                .1
                                .select_row(Some(&combined_matches[index - 1].0));
                        } else {
                            combined_matches[combined_matches.len() - 1]
                                .1
                                .select_row(Some(&combined_matches[combined_matches.len() - 1].0));
                        }
                    }
                    _ => unreachable!(),
                }

                Inhibit(true)
            }
            // Handle when the selected match is "activated"
            constants::Return => {
                let mut _runtime_data_clone = runtime_data_clone.borrow_mut();

                let (selected_match, plugin_view) = match _runtime_data_clone
                    .as_ref()
                    .unwrap()
                    .plugins
                    .iter()
                    .find_map(|view| view.list.selected_row().map(|row| (row, view)))
                {
                    Some(selected) => selected,
                    None => {
                        return Inhibit(false);
                    }
                };

                // Perform actions based on the result of handling the selection
                match plugin_view.plugin.handle_selection()(unsafe {
                    (*selected_match.data::<Match>("match").unwrap().as_ptr()).clone()
                }) {
                    HandleResult::Close => {
                        window.close();
                        Inhibit(true)
                    }
                    HandleResult::Refresh(exclusive) => {
                        if exclusive {
                            _runtime_data_clone.as_mut().unwrap().exclusive =
                                Some(plugin_view.clone());
                        } else {
                            _runtime_data_clone.as_mut().unwrap().exclusive = None;
                        }
                        mem::drop(_runtime_data_clone); // Drop the mutable borrow
                        refresh_matches(entry_clone.text().into(), runtime_data_clone.clone());
                        Inhibit(false)
                    }
                    HandleResult::Copy(bytes) => {
                        _runtime_data_clone.as_mut().unwrap().post_run_action =
                            PostRunAction::Copy(bytes.into());
                        window.close();
                        Inhibit(true)
                    }
                    HandleResult::Stdout(bytes) => {
                        if let Err(why) = io::stdout().lock().write_all(&bytes) {
                            eprintln!("Error outputting content to stdout: {}", why);
                        }
                        window.close();
                        Inhibit(true)
                    }
                }
            }
            _ => Inhibit(false),
        }
    });

    // If the option is enabled, close the window when any click is received
    // that is outside the bounds of the main box
    if runtime_data
        .borrow()
        .as_ref()
        .unwrap()
        .config
        .close_on_click
    {
        window.connect_button_press_event(move |window, event| {
            if event.window() == window.window() {
                window.close();
                Inhibit(true)
            } else {
                Inhibit(false)
            }
        });
    }

    // Create widgets here for proper positioning
    window.connect_configure_event(move |window, event| {
        let width = match runtime_data.borrow().as_ref().unwrap().config.width {
            RelativeNum::Absolute(width) => width,
            RelativeNum::Fraction(fraction) => (event.size().0 as f32 * fraction) as i32,
        };
        // The GtkFixed widget is used for absolute positioning of the main box
        let fixed = gtk::Fixed::builder().build();
        let main_vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .halign(gtk::Align::Center)
            .vexpand(false)
            .width_request(width)
            .name(style_names::MAIN)
            .build();
        main_vbox.add(&entry);

        // Display the error message
        if !runtime_data
            .borrow()
            .as_ref()
            .unwrap()
            .error_label
            .is_empty()
        {
            main_vbox.add(
                &gtk::Label::builder()
                    .label(&format!(
                        r#"<span foreground="red">{}</span>"#,
                        runtime_data.borrow().as_ref().unwrap().error_label
                    ))
                    .use_markup(true)
                    .build(),
            );
        }

        let vertical_offset = match runtime_data
            .borrow()
            .as_ref()
            .unwrap()
            .config
            .vertical_offset
        {
            RelativeNum::Absolute(offset) => offset,
            RelativeNum::Fraction(fraction) => (event.size().1 as f32 * fraction) as i32,
        };

        fixed.put(
            &main_vbox,
            (event.size().0 as i32 - width) / 2,
            match runtime_data.borrow().as_ref().unwrap().config.position {
                Position::Top => vertical_offset,
                Position::Center => {
                    (event.size().1 as i32 - entry.allocated_height()) / 2 + vertical_offset
                }
            },
        );
        window.add(&fixed);
        window.show_all();

        // Add and show the list later, to avoid showing empty plugin categories on launch
        main_vbox.add(&main_list);
        main_list.show();
        entry.grab_focus(); // Grab the focus so typing is immediately accepted by the entry box
        false
    });

    // Show the window initially, so it gets allocated and configured
    window.show_all();
}

fn handle_matches(plugin_view: PluginView, runtime_data: &RuntimeData, matches: RVec<Match>) {
    // Clear out the old matches from the list
    for widget in plugin_view.list.children() {
        plugin_view.list.remove(&widget);
    }

    // If there are no matches, hide the plugin's results
    if matches.is_empty() {
        plugin_view.row.hide();
        return;
    }

    for _match in matches {
        let hbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .name(style_names::MATCH)
            .hexpand(true)
            .build();
        if !runtime_data.config.hide_icons {
            if let ROption::RSome(icon) = &_match.icon {
                let mut builder = gtk::Image::builder()
                    .name(style_names::MATCH)
                    .pixel_size(32);

                let path = PathBuf::from(icon.as_str());

                // If the icon path is absolute, load that file
                if path.is_absolute() {
                    match gdk_pixbuf::Pixbuf::from_file_at_size(icon.as_str(), 32, 32) {
                        Ok(pixbuf) => builder = builder.pixbuf(&pixbuf),
                        Err(why) => {
                            println!("Failed to load icon file: {}", why);
                            builder = builder.icon_name("image-missing"); // Set "broken" icon
                        }
                    }
                } else {
                    builder = builder.icon_name(icon);
                }

                hbox.add(&builder.build());
            }
        }
        let title = gtk::Label::builder()
            .name(style_names::MATCH_TITLE)
            .wrap(true)
            .xalign(0.0)
            .use_markup(_match.use_pango)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .label(&_match.title)
            .build();

        // If a description is present, make a box with it and the title
        match &_match.description {
            ROption::RSome(desc) => {
                let title_desc_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .name(style_names::MATCH)
                    .hexpand(true)
                    .vexpand(true)
                    .build();
                title_desc_box.add(&title);
                title_desc_box.add(
                    &gtk::Label::builder()
                        .name(style_names::MATCH_DESC)
                        .wrap(true)
                        .use_markup(_match.use_pango)
                        .halign(gtk::Align::Start)
                        .valign(gtk::Align::Center)
                        .label(desc)
                        .build(),
                );
                hbox.add(&title_desc_box);
            }
            ROption::RNone => {
                hbox.add(&title);
            }
        }
        let row = gtk::ListBoxRow::builder()
            .name(style_names::MATCH)
            .height_request(32)
            .build();
        row.add(&hbox);
        // GTK data setting is not type checked, so it is unsafe.
        // Only `Match` objects are stored though.
        unsafe {
            row.set_data("match", _match);
        }
        plugin_view.list.add(&row);
    }

    // Refresh the items in the view
    plugin_view.row.show_all();

    let combined_matches = runtime_data
        .plugins
        .iter()
        .flat_map(|view| {
            view.list
                .children()
                .into_iter()
                .map(move |child| (child.dynamic_cast::<gtk::ListBoxRow>().unwrap(), view))
        })
        .collect::<Vec<(gtk::ListBoxRow, &PluginView)>>();

    // If `max_entries` is set, truncate the amount of entries
    if let Some(max_matches) = runtime_data.config.max_entries {
        for (row, view) in combined_matches.iter().skip(max_matches) {
            view.list.remove(row);
        }
    }

    // Hide the plugins that no longer have any entries
    for (_, view) in &combined_matches {
        if view.list.children().is_empty() {
            view.row.hide();
        }
    }

    if let Some((row, view)) = combined_matches.get(0) {
        view.list.select_row(Some(row));
    }
}

/// Refresh the matches from the plugins
fn refresh_matches(input: String, runtime_data: Rc<RefCell<Option<RuntimeData>>>) {
    for plugin_view in runtime_data.borrow().as_ref().unwrap().plugins.iter() {
        let id = plugin_view.plugin.get_matches()(input.clone().into());
        let plugin_view = plugin_view.clone();
        let runtime_data_clone = runtime_data.clone();
        // If the input is empty, skip getting matches and just clear everything out.
        if input.is_empty() {
            handle_matches(
                plugin_view,
                runtime_data.borrow().as_ref().unwrap(),
                RVec::new(),
            );
        // If a plugin has requested exclusivity, respect it
        } else if let Some(exclusive) = &runtime_data.borrow().as_ref().unwrap().exclusive {
            if plugin_view.plugin.info() == exclusive.plugin.info() {
                glib::timeout_add_local(Duration::from_micros(1000), move || {
                    async_match(plugin_view.clone(), runtime_data_clone.clone(), id)
                });
            } else {
                handle_matches(
                    plugin_view.clone(),
                    runtime_data.borrow().as_ref().unwrap(),
                    RVec::new(),
                );
            }
        } else {
            glib::timeout_add_local(Duration::from_micros(1000), move || {
                async_match(plugin_view.clone(), runtime_data_clone.clone(), id)
            });
        }
    }
}

/// Handle the asynchronously running match task
fn async_match(
    plugin_view: PluginView,
    runtime_data: Rc<RefCell<Option<RuntimeData>>>,
    id: u64,
) -> glib::Continue {
    match plugin_view.plugin.poll_matches()(id) {
        PollResult::Ready(matches) => {
            handle_matches(
                plugin_view,
                runtime_data.borrow().as_ref().unwrap(),
                matches,
            );
            glib::Continue(false)
        }
        PollResult::Pending => glib::Continue(true),
        PollResult::Cancelled => glib::Continue(false),
    }
}
