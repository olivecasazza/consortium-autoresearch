//! Event handler traits.
//!
//! Rust implementation of `ClusterShell.Event`.

use std::any::Any;

/// Trait for handling worker lifecycle events.
///
/// Implement any subset of these methods to react to events
/// from workers and tasks. All methods have default no-op implementations.
pub trait EventHandler: Send + Sync {
    /// Called to indicate that a worker has just started.
    fn ev_start(&mut self, _worker: &dyn Any) {}

    /// Called for each node to indicate that a worker command for a
    /// specific node (or key) has just started.
    fn ev_pickup(&mut self, _worker: &dyn Any, _node: &str) {}

    /// Called to indicate that a worker has data to read from a specific
    /// node (or key).
    fn ev_read(&mut self, _worker: &dyn Any, _node: &str, _sname: &str, _msg: &str) {}

    /// Called to indicate that a worker has error to read on stderr from
    /// a specific node (or key).
    ///
    /// DEPRECATED: use ev_read instead and check if sname is 'stderr'
    fn ev_error(&mut self, _worker: &dyn Any) {}

    /// Called to indicate that some writing has been done by the worker to a
    /// node on a given stream.
    fn ev_written(&mut self, _worker: &dyn Any, _node: &str, _sname: &str, _size: usize) {}

    /// Called for each node to indicate that a worker command for a specific
    /// node has just finished.
    fn ev_hup(&mut self, _worker: &dyn Any, _node: &str, _rc: i32) {}

    /// Called to indicate that a worker has just finished.
    fn ev_close(&mut self, _worker: &dyn Any, _timedout: bool) {}

    /// Routing event (private). Called to indicate that a (meta)worker has just
    /// updated one of its route path.
    fn _ev_routing(&mut self, _worker: &dyn Any, _arg: &str) {}

    /// Called to indicate that a message has been received on an EnginePort.
    fn ev_msg(&mut self, _port: u16, _msg: &str) {}

    /// Called to indicate that a timer is firing.
    fn ev_timer(&mut self, _timer: &dyn Any) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that default implementations are callable for all methods
    #[test]
    fn test_default_impls_callable() {
        struct DefaultHandler;

        impl EventHandler for DefaultHandler {}

        let mut handler = DefaultHandler;

        // These should not panic - testing default no-op impls
        handler.ev_start(&dyn_any_placeholder());
        handler.ev_pickup(&dyn_any_placeholder(), "node1");
        handler.ev_read(&dyn_any_placeholder(), "node1", "stdout", "hello");
        handler.ev_error(&dyn_any_placeholder());
        handler.ev_written(&dyn_any_placeholder(), "node1", "stdout", 100);
        handler.ev_hup(&dyn_any_placeholder(), "node1", 0);
        handler.ev_close(&dyn_any_placeholder(), false);
        handler._ev_routing(&dyn_any_placeholder(), "route_data");
        handler.ev_msg(8080, "message");
        handler.ev_timer(&dyn_any_placeholder());
    }

    /// Test that custom implementations can override specific methods
    #[test]
    fn test_custom_overrides() {
        struct CustomHandler {
            pub start_count: usize,
            pub close_count: usize,
        }

        impl CustomHandler {
            fn new() -> Self {
                Self {
                    start_count: 0,
                    close_count: 0,
                }
            }
        }

        impl EventHandler for CustomHandler {
            fn ev_start(&mut self, _worker: &dyn Any) {
                self.start_count += 1;
            }

            fn ev_close(&mut self, _worker: &dyn Any, timedout: bool) {
                self.close_count += 1;
                assert!(!timedout, "Expected no timeout in this test");
            }
        }

        let mut handler = CustomHandler::new();

        handler.ev_start(&dyn_any_placeholder());
        handler.ev_start(&dyn_any_placeholder());
        handler.ev_close(&dyn_any_placeholder(), false);

        assert_eq!(handler.start_count, 2, "ev_start should be called twice");
        assert_eq!(handler.close_count, 1, "ev_close should be called once");
    }

    /// Test that the trait is object-safe and can be used as Box<dyn EventHandler>
    ///
    /// This test verifies that EventHandler is object-safe by:
    /// 1. Creating Box<dyn EventHandler> from a concrete type
    /// 2. Calling methods through the trait object
    /// 3. Storing multiple handlers in a Vec<dyn EventHandler>
    #[test]
    fn test_object_safety() {
        // Each handler records into its own shared cell, so the assertions can
        // read what happened through the trait object rather than by holding a
        // concrete handle the caller no longer owns.
        // EventHandler: Send, so the shared state must be Arc, not Rc.
        let starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        struct Handler1 {
            starts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl EventHandler for Handler1 {
            fn ev_start(&mut self, _worker: &dyn Any) {
                self.starts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        struct Handler2 {
            reads: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }

        impl EventHandler for Handler2 {
            fn ev_read(&mut self, _worker: &dyn Any, node: &str, sname: &str, msg: &str) {
                self.reads
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}:{}", node, sname, msg));
            }
        }

        // This demonstrates object safety - we can store different handler types
        // in a Vec of trait objects
        let mut handlers: Vec<Box<dyn EventHandler>> = Vec::new();
        handlers.push(Box::new(Handler1 {
            starts: starts.clone(),
        }));
        handlers.push(Box::new(Handler2 {
            reads: reads.clone(),
        }));

        // Call ev_start on all handlers
        for handler in &mut handlers {
            handler.ev_start(&dyn_any_placeholder());
        }

        // Call ev_read on all handlers
        for handler in &mut handlers {
            handler.ev_read(&dyn_any_placeholder(), "node1", "stdout", "test");
        }

        // The fact that we can compile and run this code proves that
        // EventHandler is object-safe. The trait object can be used
        // polymorphically — assert the calls actually reached each concrete
        // handler through the `dyn EventHandler` indirection.
        assert_eq!(handlers.len(), 2);
        assert_eq!(
            starts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "ev_start should reach the single handler that implements it"
        );
        assert_eq!(
            reads.lock().unwrap().as_slice(),
            ["node1:stdout:test"],
            "ev_read should reach the handler that implements it"
        );
    }

    /// Helper to create a placeholder for dyn Any
    fn dyn_any_placeholder() -> Box<dyn Any> {
        Box::new(())
    }

    /// Test that EventHandler is Send and Sync (required for Box<dyn EventHandler>)
    #[test]
    fn test_send_sync() {
        struct SendSyncHandler;

        impl EventHandler for SendSyncHandler {}

        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}

        assert_send::<Box<dyn EventHandler>>();
        assert_sync::<Box<dyn EventHandler>>();
        assert_send::<SendSyncHandler>();
        assert_sync::<SendSyncHandler>();
    }
}
