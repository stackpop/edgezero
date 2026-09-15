use std::{collections::BTreeMap, env, process};

fn main() {
    let expected = BTreeMap::from([
        ("HOME".to_owned(), "/work/home".to_owned()),
        (
            "PATH".to_owned(),
            "/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin".to_owned(),
        ),
        ("TMPDIR".to_owned(), "/work/tmp".to_owned()),
    ]);
    if env::vars().collect::<BTreeMap<_, _>>() != expected {
        eprintln!("unexpected runtime environment");
        process::exit(2);
    }
    if env::args().skip(1).collect::<Vec<_>>() != ["--help"] {
        eprintln!("unexpected runtime arguments");
        process::exit(3);
    }
    println!("edgezero image runtime smoke");
}
