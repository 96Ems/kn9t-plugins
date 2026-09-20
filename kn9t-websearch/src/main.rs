fn main() {
    kn9t_plugin_sdk::Plugin::new("kn9t-websearch")
        .tool(websearch::WebSearch)
        .tool(scrape::Scrape)
        .run();
}

mod http;
mod scrape;
mod websearch;
