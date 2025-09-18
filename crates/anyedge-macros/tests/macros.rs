use anyedge_controller::{ControllerHandler, Path, RouteSet};
use anyedge_core::{App, Method, Request};
use anyedge_macros::action;
use futures::executor::block_on;

#[derive(serde::Deserialize)]
struct SlugParams {
    slug: String,
}

#[action]
async fn list(path: Path<SlugParams>) -> impl anyedge_controller::Responder {
    let Path(params) = path;
    anyedge_controller::Text::new(format!("list:{}", params.slug))
}

#[action]
async fn create() -> impl anyedge_controller::Responder {
    anyedge_controller::Text::new("create")
}

fn handler_output(handler: ControllerHandler, req: Request) -> String {
    let res = block_on(handler.call(req));
    String::from_utf8(res.body).unwrap()
}

#[test]
fn action_macro_builds_controller_handler() {
    let handler = list();
    let mut req = Request::new(Method::GET, "/notes/demo");
    req.params.insert("slug".into(), "demo".into());
    let body = handler_output(handler, req);
    assert_eq!(body, "list:demo");
}

#[test]
fn action_handlers_can_register_routes() {
    let mut app = App::new();
    RouteSet::new()
        .add("/notes/:slug", anyedge_controller::get(list()))
        .add("/notes", anyedge_controller::post(create()))
        .apply(&mut app);

    let mut req = Request::new(Method::GET, "/notes/demo");
    req.params.insert("slug".into(), "demo".into());
    let res = block_on(app.handle(req));
    assert_eq!(String::from_utf8(res.body).unwrap(), "list:demo");

    let res = block_on(app.handle(Request::new(Method::POST, "/notes")));
    assert_eq!(String::from_utf8(res.body).unwrap(), "create");
}
