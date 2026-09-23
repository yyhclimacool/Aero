use gpui::{Context, Pixels, Task, px};
use instant::Duration;

static INTERVAL: Duration = Duration::from_millis(500);
static PAUSE_DELAY: Duration = Duration::from_millis(300);

// On Windows, Linux, we should use integer to avoid blurry cursor.
#[cfg(not(target_os = "macos"))]
pub(super) const CURSOR_WIDTH: Pixels = px(2.);
#[cfg(target_os = "macos")]
pub(super) const CURSOR_WIDTH: Pixels = px(1.5);

/// To manage the Input cursor blinking.
///
/// It will start blinking with a interval of 500ms.
/// Every loop will notify the view to update the `visible`, and Input will observe this update to touch repaint.
///
/// The input painter will check if this in visible state, then it will draw the cursor.
///
/// Only a started cursor (its input is focused) blinks or schedules timers: an
/// unfocused input must stay idle, otherwise every such input repaints the
/// window twice a second forever. Edits still pause it (the caret shows as soon
/// as the input gets focus), they just leave no timer behind.
pub(crate) struct BlinkCursor {
    visible: bool,
    paused: bool,
    active: bool,
    epoch: usize,

    _task: Task<()>,
}

impl BlinkCursor {
    pub(crate) fn new() -> Self {
        Self {
            visible: false,
            paused: false,
            active: false,
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    /// Start the blinking
    pub(crate) fn start(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.paused = false;
        self.blink(self.epoch, cx);
    }

    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        self.active = false;
        self.paused = false;
        self.epoch = 0;
        self._task = Task::ready(());
        cx.notify();
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    fn blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        if self.paused || epoch != self.epoch {
            self.visible = true;
            return;
        }

        self.visible = !self.visible;
        cx.notify();

        // Schedule the next blink
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(INTERVAL).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.blink(epoch, cx));
            }
        });
    }

    pub(crate) fn visible(&self) -> bool {
        // Keep showing the cursor if paused
        self.paused || self.visible
    }

    /// Show the cursor immediately and restart the idle delay before blinking resumes.
    pub(crate) fn pause(&mut self, cx: &mut Context<Self>) {
        self.paused = true;
        self.visible = true;
        cx.notify();
        if !self.active {
            return;
        }

        // Every pause replaces the pending timer, keeping repeated input visible.
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE_DELAY).await;

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    this.paused = false;
                    this.blink(epoch, cx);
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};

    #[gpui::test]
    fn repeated_pauses_keep_cursor_visible_until_idle(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        cx.executor().advance_clock(INTERVAL);
        cx.run_until_parked();
        for _ in 0..5 {
            cursor.update(cx, |cursor, cx| cursor.pause(cx));
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
        }
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
        cx.executor().advance_clock(INTERVAL);
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
    }

    /// Edits pause the cursor; an input that is not focused must not start blinking
    /// from that, and a stopped cursor must not keep a timer alive.
    #[gpui::test]
    fn unfocused_cursor_never_schedules_blinks(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        let notified = std::rc::Rc::new(std::cell::Cell::new(0));
        let _observer = cx.update({
            let notified = notified.clone();
            |cx| cx.observe(&cursor, move |_, _| notified.set(notified.get() + 1))
        });
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.run_until_parked();
        let paused = notified.get();
        cx.executor().advance_clock(INTERVAL * 10);
        cx.run_until_parked();
        assert_eq!(notified.get(), paused, "unfocused input schedules nothing");
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()), "the edit still reveals the caret");

        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.executor().advance_clock(INTERVAL * 4);
        cx.run_until_parked();
        assert!(notified.get() >= 4, "focused input blinks");
        cursor.update(cx, |cursor, cx| cursor.stop(cx));
        cx.run_until_parked();
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.run_until_parked();
        let stopped = notified.get();
        cx.executor().advance_clock(INTERVAL * 10);
        cx.run_until_parked();
        assert_eq!(notified.get(), stopped, "blurred input schedules nothing");
    }
}
