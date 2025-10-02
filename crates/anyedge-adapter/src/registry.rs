use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::RwLock;

/// Actions the AnyEdge CLI can request from an adapter implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterAction {
    Build,
    Deploy,
    Serve,
}

/// Interface implemented by adapter crates to integrate with the AnyEdge CLI.
pub trait Adapter: Sync + Send {
    /// Name used to reference the adapter (case-insensitive).
    fn name(&self) -> &'static str;

    /// Execute the requested action with optional adapter-specific args.
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String>;
}

static REGISTRY: Lazy<RwLock<HashMap<String, &'static dyn Adapter>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Registers an adapter so it can be discovered by the CLI.
pub fn register_adapter(adapter: &'static dyn Adapter) {
    let mut registry = REGISTRY
        .write()
        .expect("anyedge adapter registry lock poisoned");
    registry.insert(adapter.name().to_ascii_lowercase(), adapter);
}

/// Looks up an adapter by name.
pub fn get_adapter(name: &str) -> Option<&'static dyn Adapter> {
    let registry = REGISTRY
        .read()
        .expect("anyedge adapter registry lock poisoned");
    registry.get(&name.to_ascii_lowercase()).copied()
}

/// Returns the names of all registered adapters.
pub fn registered_adapters() -> Vec<String> {
    let registry = REGISTRY
        .read()
        .expect("anyedge adapter registry lock poisoned");
    let mut names: Vec<String> = registry.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static HIT: AtomicUsize = AtomicUsize::new(0);
    static TEST_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    struct TestAdapter {
        name: &'static str,
        hit_value: usize,
    }

    impl Adapter for TestAdapter {
        fn name(&self) -> &'static str {
            self.name
        }

        fn execute(&self, _action: AdapterAction, _args: &[String]) -> Result<(), String> {
            HIT.store(self.hit_value, Ordering::SeqCst);
            Ok(())
        }
    }

    static FIRST: TestAdapter = TestAdapter {
        name: "dummy",
        hit_value: 1,
    };
    static SECOND: TestAdapter = TestAdapter {
        name: "dummy",
        hit_value: 2,
    };
    static OTHER: TestAdapter = TestAdapter {
        name: "other",
        hit_value: 3,
    };

    fn reset() {
        let mut registry = super::REGISTRY.write().expect("registry lock");
        registry.clear();
        HIT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn registers_and_fetches_adapter() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset();
        register_adapter(&FIRST);
        let adapter = get_adapter("dummy").expect("adapter present");
        adapter
            .execute(AdapterAction::Build, &[])
            .expect("execute succeeds");
        assert_eq!(HIT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn latest_registration_overrides_previous() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset();
        register_adapter(&FIRST);
        register_adapter(&SECOND);
        let adapter = get_adapter("dummy").expect("adapter present");
        adapter
            .execute(AdapterAction::Deploy, &[])
            .expect("execute succeeds");
        assert_eq!(HIT.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn registered_adapters_are_sorted() {
        let _guard = TEST_LOCK.lock().expect("lock");
        reset();
        register_adapter(&OTHER);
        register_adapter(&FIRST);
        let adapters = registered_adapters();
        assert_eq!(adapters, vec!["dummy".to_string(), "other".to_string()]);
    }
}
