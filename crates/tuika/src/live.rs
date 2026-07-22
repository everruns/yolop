//! Small observable values for data-driven views.
//!
//! `Live` is not a reconciler and does not spawn work. Producers own their
//! threads or async tasks; updating a value requests a redraw from a connected
//! [`Runner`](crate::Runner), and views read the latest value each frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use ratatui::layout::Rect;

use crate::{Element, RenderCtx, Size, Surface, View};

/// A thread-safe request for the terminal runner to redraw.
#[derive(Clone, Default)]
pub struct RedrawHandle {
    requested: Arc<AtomicBool>,
}

impl RedrawHandle {
    /// Mark the connected runner dirty.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub(crate) fn take(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }
}

/// Shared application data whose updates invalidate a connected runner.
pub struct Live<T> {
    value: Arc<RwLock<T>>,
    redraw: Option<RedrawHandle>,
}

impl<T> Clone for Live<T> {
    fn clone(&self) -> Self {
        Self {
            value: Arc::clone(&self.value),
            redraw: self.redraw.clone(),
        }
    }
}

impl<T> Live<T> {
    /// Create a value that is read live but does not notify a runner.
    pub fn new(value: T) -> Self {
        Self {
            value: Arc::new(RwLock::new(value)),
            redraw: None,
        }
    }

    /// Create a value whose mutations request redraws through `redraw`.
    pub fn with_redraw(value: T, redraw: RedrawHandle) -> Self {
        Self {
            value: Arc::new(RwLock::new(value)),
            redraw: Some(redraw),
        }
    }

    /// Read the current value without exposing the lock guard.
    pub fn with<R>(&self, read: impl FnOnce(&T) -> R) -> R {
        let value = self.value.read().unwrap_or_else(|error| error.into_inner());
        read(&value)
    }

    /// Mutate the value and request a redraw after releasing the write lock.
    pub fn update<R>(&self, update: impl FnOnce(&mut T) -> R) -> R {
        let result = {
            let mut value = self
                .value
                .write()
                .unwrap_or_else(|error| error.into_inner());
            update(&mut value)
        };
        if let Some(redraw) = &self.redraw {
            redraw.request();
        }
        result
    }

    /// Replace the current value and request a redraw.
    pub fn set(&self, value: T) {
        self.update(|current| *current = value);
    }
}

/// A view derived from the current value of a [`Live`] on every measurement
/// and render pass.
pub struct LiveView<T, F> {
    value: Live<T>,
    build: F,
}

impl<T, F> LiveView<T, F>
where
    F: Fn(&T) -> Element,
{
    /// Bind `value` to a view-building function.
    pub fn new(value: Live<T>, build: F) -> Self {
        Self { value, build }
    }

    fn current(&self) -> Element {
        self.value.with(&self.build)
    }
}

impl<T, F> View for LiveView<T, F>
where
    F: Fn(&T) -> Element,
{
    fn measure(&self, available: Size) -> Size {
        self.current().measure(available)
    }

    fn render(&self, area: Rect, surface: &mut Surface, ctx: &RenderCtx) {
        self.current().render(area, surface, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::Text;
    use crate::style::Theme;
    use crate::test_support::row;
    use crate::view::element;

    #[test]
    fn live_view_reads_updated_data_without_reconstruction() {
        let redraw = RedrawHandle::default();
        let value = Live::with_redraw(1u32, redraw.clone());
        let view = LiveView::new(value.clone(), |value| {
            element(Text::raw(format!("count: {value}")))
        });
        let theme = Theme::default();
        assert_eq!(
            row(&crate::testing::render(&view, 12, 1, &theme), 0),
            "count: 1"
        );
        value.set(2);
        assert!(redraw.take());
        assert_eq!(
            row(&crate::testing::render(&view, 12, 1, &theme), 0),
            "count: 2"
        );
    }
}
