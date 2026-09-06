//! Example: Hello World Noxisa App

use nexus_sdk::{App, geometry::Rect, color::Color, event::Event};

fn main() {
    let mut app = App::new("com.example.hello");

    {
        let win = app.create_window("Hello Noxisa", Rect::new(100, 100, 800, 600));
        win.set_background(0x0f1117);

        win.on_event(|event| {
            match event {
                Event::KeyPress(k) => println!("Key pressed: {}", k.key),
                Event::CloseRequest => println!("Closing..."),
                _ => {}
            }
        });

        win.show();
    }

    // AI completion example
    let ai = nexus_sdk::ai::AiClient::new();
    let completion = ai.complete("fn main() {\n    let x = Vec::", "rust");
    println!("AI suggests: {completion}");

    app.run();
}
