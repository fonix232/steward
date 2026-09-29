//! ubus-call <object> <method> [json]: `ubus call`, through steward-ubus.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let args = a.get(3).map_or(serde_json::json!({}), |j| {
        serde_json::from_str(j).expect("json")
    });
    let mut u = steward_ubus::Ubus::connect().expect("connect");
    match u.call(&a[1], &a[2], args.as_object().expect("object")) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap()),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1)
        }
    }
}
