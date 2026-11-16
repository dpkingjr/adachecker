//! Embedded browser preview (WKWebView on macOS via `wry`) with finding highlighting.
//!
//! The webview is a native child view layered over the egui window. Each frame the
//! app tells it which rectangle of the window to occupy (`place`) or to hide (`hide`).
//! Selecting a finding injects a small script that locates the element in the live
//! DOM, scrolls it into view, and draws an outline with a label over it.

use crate::checks::{Finding, Locator, Severity};
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use url::Url;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder};

/// Defines `window.__adaChecker` in the page (idempotent).
const HIGHLIGHT_JS: &str = r#"
(function () {
  if (window.__adaChecker) return;
  var CLS = '__ada_checker_overlay';
  var FONT = '600 13px -apple-system, BlinkMacSystemFont, system-ui, sans-serif';
  // The selected element's overlay; repositioned whenever the layout changes.
  var current = null;
  function clear() {
    current = null;
    document.querySelectorAll('.' + CLS).forEach(function (e) { e.remove(); });
  }
  function find(loc) {
    var tag = (loc.tag || '').toLowerCase();
    function ok(el) { return el && (!tag || el.localName.toLowerCase() === tag); }
    // An id only identifies the element if it is unique; duplicate ids (themselves a
    // finding) fall through to the CSS path.
    if (loc.id) {
      var e = document.getElementById(loc.id), unique = false;
      try { unique = document.querySelectorAll('[id="' + CSS.escape(loc.id) + '"]').length === 1; } catch (_) {}
      if (unique && ok(e)) return e;
    }
    if (loc.path) { try { var p = document.querySelector(loc.path); if (ok(p)) return p; } catch (_) {} }
    if (loc.attr && tag) {
      var all = document.getElementsByTagName(tag);
      for (var i = 0; i < all.length; i++) if (all[i].getAttribute(loc.attr[0]) === loc.attr[1]) return all[i];
    }
    return null;
  }
  function banner(text, color) {
    var b = document.createElement('div');
    b.className = CLS;
    b.setAttribute('aria-hidden', 'true');
    b.textContent = text;
    Object.assign(b.style, {
      position: 'fixed', left: '8px', right: '8px', top: '8px', zIndex: '2147483647',
      background: color, color: '#fff', font: FONT, lineHeight: '1.4', padding: '10px 14px',
      borderRadius: '6px', boxShadow: '0 2px 10px rgba(0,0,0,.4)', pointerEvents: 'none'
    });
    document.documentElement.appendChild(b);
  }
  function visibleBox(el) {
    var r = el.getBoundingClientRect();
    var proxy = false;
    while (el && r.width === 0 && r.height === 0) {
      el = el.parentElement; proxy = true;
      if (el) r = el.getBoundingClientRect();
    }
    return { rect: r, proxy: proxy };
  }
  function placeHighlight() {
    if (!current || !current.el.isConnected) return;
    var el = current.el, r = visibleBox(el).rect;
    // While the page is still settling (late images, fonts, scripts), keep the element in view.
    if (Date.now() < current.until && (r.bottom < 0 || r.top > window.innerHeight)) {
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      r = visibleBox(el).rect;
    }
    var sx = window.scrollX, sy = window.scrollY;
    Object.assign(current.o.style, {
      left: (r.left + sx - 4) + 'px', top: (r.top + sy - 4) + 'px',
      width: (r.width + 8) + 'px', height: (r.height + 8) + 'px'
    });
    var t = current.t, top = r.top + sy - 8 - t.offsetHeight;
    if (top < sy) top = r.bottom + sy + 8;
    Object.assign(t.style, { left: Math.max(sx + 4, r.left + sx - 4) + 'px', top: top + 'px' });
  }
  function draw(el, label, color) {
    var o = document.createElement('div');
    o.className = CLS;
    o.setAttribute('aria-hidden', 'true');
    Object.assign(o.style, {
      position: 'absolute', boxSizing: 'border-box',
      border: '3px solid ' + color, background: color + '22', borderRadius: '4px',
      boxShadow: '0 0 0 4px rgba(255,255,255,.85), 0 0 0 100000px rgba(0,0,0,.28)',
      zIndex: '2147483646', pointerEvents: 'none', margin: '0'
    });
    var t = document.createElement('div');
    t.className = CLS;
    t.setAttribute('aria-hidden', 'true');
    t.textContent = (visibleBox(el).proxy ? '[element has no visible box; showing nearest visible ancestor] ' : '') + label;
    Object.assign(t.style, {
      position: 'absolute', maxWidth: '560px', background: color, color: '#fff', font: FONT, lineHeight: '1.35',
      padding: '5px 9px', borderRadius: '4px', zIndex: '2147483647', pointerEvents: 'none',
      boxShadow: '0 2px 8px rgba(0,0,0,.35)', margin: '0'
    });
    document.documentElement.appendChild(o);
    document.documentElement.appendChild(t);
    current = { el: el, o: o, t: t, until: Date.now() + 4000 };
    placeHighlight();
    watchLayout();
  }
  // Repositions overlays when the page's layout shifts (images loading, reflow, resize).
  var watching = false;
  function relayout() { placeHighlight(); placeMarkers(); }
  function watchLayout() {
    if (watching) return;
    watching = true;
    window.addEventListener('resize', relayout);
    if (window.ResizeObserver) {
      var ro = new ResizeObserver(function () { relayout(); });
      ro.observe(document.documentElement);
      if (document.body) ro.observe(document.body);
    }
    setInterval(relayout, 500);
  }
  function show(loc, label, color) {
    clear();
    if (!loc) { banner('Page-level issue — ' + label, color); return; }
    var el = find(loc);
    if (!el) {
      banner('Could not find this element in the live page (it may have been changed by JavaScript). ' + label, '#555');
      return;
    }
    if (el.closest('head')) { banner('Element is in <head> and is not rendered — ' + label, color); return; }
    el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
    requestAnimationFrame(function () { draw(el, label, color); });
  }
  // ----- Markers for every finding: dashed outline + clickable badge per element -----
  var MCLS = '__ada_checker_marker';
  var marks = { groups: [], visible: true };
  function placeMarkers() {
    var sx = window.scrollX, sy = window.scrollY;
    marks.groups.forEach(function (g) {
      var r = g.el.isConnected ? g.el.getBoundingClientRect() : null;
      var on = marks.visible && r && (r.width > 0 || r.height > 0);
      g.box.style.display = on ? 'block' : 'none';
      g.badge.style.display = on ? 'block' : 'none';
      if (!on) return;
      Object.assign(g.box.style, {
        left: (r.left + sx - 2) + 'px', top: (r.top + sy - 2) + 'px',
        width: (r.width + 4) + 'px', height: (r.height + 4) + 'px'
      });
      Object.assign(g.badge.style, {
        left: Math.max(sx, r.left + sx - 2) + 'px', top: Math.max(0, r.top + sy - 20) + 'px'
      });
    });
  }
  function mark(items) {
    marks.groups.forEach(function (g) { g.box.remove(); g.badge.remove(); });
    var byEl = new Map();
    items.forEach(function (it) {
      var el = it.loc && find(it.loc);
      if (!el || el.closest('head')) return;
      var g = byEl.get(el);
      if (!g) { g = { el: el, items: [], error: false }; byEl.set(el, g); }
      g.items.push(it);
      if (it.error) g.error = true;
    });
    marks.groups = [];
    byEl.forEach(function (g) {
      g.items.sort(function (a, b) { return (b.error ? 1 : 0) - (a.error ? 1 : 0); });
      var color = g.error ? '#c62828' : '#9a5b00';
      g.box = document.createElement('div');
      g.box.className = MCLS;
      g.box.setAttribute('aria-hidden', 'true');
      Object.assign(g.box.style, {
        position: 'absolute', boxSizing: 'border-box', border: '2px dashed ' + color,
        borderRadius: '3px', pointerEvents: 'none', zIndex: '2147483640', margin: '0'
      });
      g.badge = document.createElement('button');
      g.badge.type = 'button';
      g.badge.className = MCLS;
      g.badge.textContent = (g.error ? '✖ ' : '⚠ ') + g.items.length;
      g.badge.title = g.items.map(function (i) { return i.label; }).join('\n') + '\n\nClick to show this element in the HTML source.';
      g.badge.setAttribute('aria-label', g.items.length + ' accessibility issue(s). ' + g.items[0].label);
      Object.assign(g.badge.style, {
        position: 'absolute', zIndex: '2147483641', font: '700 11px/16px -apple-system, system-ui, sans-serif',
        color: '#fff', background: color, border: '1px solid #fff', borderRadius: '9px', padding: '0 6px',
        margin: '0', cursor: 'pointer', boxShadow: '0 1px 4px rgba(0,0,0,.45)', width: 'auto', height: 'auto',
        minWidth: '0', textTransform: 'none', letterSpacing: 'normal', textDecoration: 'none'
      });
      g.badge.addEventListener('click', function (ev) {
        ev.preventDefault();
        ev.stopPropagation();
        if (window.ipc) window.ipc.postMessage('select:' + g.items[0].key);
      }, true);
      document.documentElement.appendChild(g.box);
      document.documentElement.appendChild(g.badge);
      marks.groups.push(g);
    });
    placeMarkers();
    watchLayout();
  }
  function setMarkersVisible(v) { marks.visible = v; placeMarkers(); }
  window.__adaChecker = { show: show, clear: clear, mark: mark, setMarkersVisible: setMarkersVisible };
})();
"#;

/// Encodes a string as a JavaScript string literal.
fn js_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn locator_js(loc: Option<&Locator>) -> String {
    let Some(l) = loc else { return "null".into() };
    let id = l.id.as_deref().map(js_str).unwrap_or_else(|| "null".into());
    let attr = l
        .attr
        .as_ref()
        .map(|(k, v)| format!("[{}, {}]", js_str(k), js_str(v)))
        .unwrap_or_else(|| "null".into());
    format!("{{tag: {}, id: {id}, path: {}, attr: {attr}}}", js_str(&l.tag), js_str(&l.path))
}

/// Parses a `select:<rule>:<finding>` IPC message from a marker badge.
fn parse_select(msg: &str) -> Option<(usize, usize)> {
    let (r, f) = msg.strip_prefix("select:")?.split_once(':')?;
    Some((r.parse().ok()?, f.parse().ok()?))
}

/// One finding to mark in the page: `(rule index, finding index, finding, criterion)`.
pub type MarkItem<'a> = (usize, usize, &'a Finding, &'a str);

fn severity_label(f: &Finding, criterion: &str) -> (String, &'static str) {
    match f.severity {
        Severity::Error => (format!("{criterion} · Error: {}", f.message), "#c62828"),
        Severity::Warning => (format!("{criterion} · Warning: {}", f.message), "#9a5b00"),
    }
}

pub struct Preview {
    webview: Option<WebView>,
    error: Option<String>,
    /// Set by the page-load handler when a navigation finishes.
    finished: Arc<AtomicBool>,
    loading: Arc<AtomicBool>,
    pending: Option<Url>,
    current_url: Option<String>,
    /// The last highlight script, re-applied after (re)loads so it survives navigation timing.
    highlight: Option<String>,
    /// Script marking every finding in the page, re-applied after (re)loads.
    markers: Option<String>,
    markers_visible: bool,
    /// Findings whose page badge was clicked, from the IPC handler.
    clicks: Arc<Mutex<Vec<(usize, usize)>>>,
    /// True between our own navigation request and its first load-finished event.
    apply_on_finish: bool,
    bounds: Option<egui::Rect>,
    visible: bool,
}

impl Preview {
    pub fn new() -> Self {
        Self {
            webview: None,
            error: None,
            finished: Arc::new(AtomicBool::new(false)),
            loading: Arc::new(AtomicBool::new(false)),
            pending: None,
            current_url: None,
            highlight: None,
            markers: None,
            markers_visible: true,
            clicks: Arc::new(Mutex::new(Vec::new())),
            apply_on_finish: false,
            bounds: None,
            visible: false,
        }
    }

    /// Whether there is anything to show (a page has been requested).
    pub fn has_content(&self) -> bool {
        self.pending.is_some() || self.current_url.is_some()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some() || self.loading.load(Ordering::Relaxed)
    }

    pub fn url(&self) -> Option<&str> {
        self.pending.as_ref().map(Url::as_str).or(self.current_url.as_deref())
    }

    /// Requests navigation; performed on the next `place` call (needs the window handle).
    pub fn navigate(&mut self, url: Url) {
        self.highlight = None;
        self.markers = None;
        self.pending = Some(url);
    }

    pub fn reload(&mut self) {
        if let Some(u) = self.current_url.as_deref().and_then(|u| Url::parse(u).ok()) {
            let keep = self.highlight.take();
            self.pending = Some(u);
            self.highlight = keep;
        }
    }

    fn ensure(&mut self, frame: &eframe::Frame, ctx: &egui::Context) -> bool {
        if self.webview.is_some() {
            return true;
        }
        if self.error.is_some() {
            return false;
        }
        let (finished, loading, clicks) = (self.finished.clone(), self.loading.clone(), self.clicks.clone());
        let (ctx, ipc_ctx) = (ctx.clone(), ctx.clone());
        let built = WebViewBuilder::new()
            // Page scripts can call window.ipc too, so only well-formed, bounds-checked
            // selections are accepted (indices are validated again by the app).
            .with_ipc_handler(move |req| {
                if let Some(key) = parse_select(req.body()) {
                    clicks.lock().unwrap_or_else(|e| e.into_inner()).push(key);
                    ipc_ctx.request_repaint();
                }
            })
            .with_visible(false)
            .with_bounds(Rect::default())
            .with_on_page_load_handler(move |event, _url| {
                match event {
                    PageLoadEvent::Started => loading.store(true, Ordering::Relaxed),
                    PageLoadEvent::Finished => {
                        loading.store(false, Ordering::Relaxed);
                        finished.store(true, Ordering::Relaxed);
                    }
                }
                ctx.request_repaint();
            })
            .build_as_child(frame);
        match built {
            Ok(w) => {
                self.webview = Some(w);
                true
            }
            Err(e) => {
                self.error = Some(e.to_string());
                false
            }
        }
    }

    fn start_pending_navigation(&mut self) {
        let (Some(url), Some(wv)) = (self.pending.take(), self.webview.as_ref()) else { return };
        let result = if url.scheme() == "file" {
            match url.to_file_path().ok().and_then(|p| std::fs::read_to_string(p).ok()) {
                Some(html) => wv.load_html(&html),
                None => wv.load_html("<p style='font:15px system-ui;padding:2em'>Could not read local file.</p>"),
            }
        } else {
            wv.load_url(url.as_str())
        };
        if let Err(e) = result {
            self.error = Some(format!("Could not load page: {e}"));
        }
        self.loading.store(true, Ordering::Relaxed);
        self.finished.store(false, Ordering::Relaxed);
        self.apply_on_finish = true;
        self.current_url = Some(url.to_string());
    }

    /// Shows the webview covering `rect` (egui points) of the window.
    pub fn place(&mut self, ctx: &egui::Context, frame: &eframe::Frame, rect: egui::Rect) {
        if !self.ensure(frame, ctx) {
            return;
        }
        self.start_pending_navigation();
        let Some(wv) = self.webview.as_ref() else { return };

        if self.bounds != Some(rect) {
            let z = ctx.zoom_factor() as f64;
            let _ = wv.set_bounds(Rect {
                position: LogicalPosition::new(rect.min.x as f64 * z, rect.min.y as f64 * z).into(),
                size: LogicalSize::new(rect.width().max(1.0) as f64 * z, rect.height().max(1.0) as f64 * z).into(),
            });
            self.bounds = Some(rect);
        }
        if !self.visible {
            let _ = wv.set_visible(true);
            self.visible = true;
        }
        // Re-apply the selected highlight once our navigation finishes, but not when the
        // user follows links inside the preview.
        if self.finished.swap(false, Ordering::Relaxed) && self.apply_on_finish {
            self.apply_on_finish = false;
            for script in self.markers.iter().chain(self.highlight.iter()) {
                let _ = wv.evaluate_script(script);
            }
        }
    }

    pub fn hide(&mut self) {
        if self.visible {
            if let Some(wv) = &self.webview {
                let _ = wv.set_visible(false);
            }
            self.visible = false;
        }
    }

    /// Whether scripts must wait for the in-flight navigation we started.
    fn deferred(&self) -> bool {
        self.pending.is_some() || (self.apply_on_finish && self.loading.load(Ordering::Relaxed))
    }

    /// Runs `script` in the page unless a navigation we started is still in flight.
    pub fn run_now_if_ready(&self, script: &str) {
        if let (Some(wv), false) = (&self.webview, self.deferred()) {
            let _ = wv.evaluate_script(script);
        }
    }

    /// Marks every element finding in the page with an outline and a clickable badge.
    pub fn set_markers(&mut self, items: &[MarkItem]) {
        let entries: Vec<String> = items
            .iter()
            .map(|(ri, fi, f, criterion)| {
                let (label, _) = severity_label(f, criterion);
                format!(
                    "{{key: \"{ri}:{fi}\", loc: {}, error: {}, label: {}}}",
                    locator_js(f.locator.as_ref()),
                    f.severity == Severity::Error,
                    js_str(&label)
                )
            })
            .collect();
        let script = format!(
            "{HIGHLIGHT_JS}\nwindow.__adaChecker.mark([{}]);\nwindow.__adaChecker.setMarkersVisible({});",
            entries.join(",\n"),
            self.markers_visible
        );
        self.run_now_if_ready(&script);
        self.markers = Some(script);
    }

    pub fn set_markers_visible(&mut self, visible: bool) {
        self.markers_visible = visible;
        if let Some(m) = &mut self.markers {
            let (on, off) = ("setMarkersVisible(true);", "setMarkersVisible(false);");
            *m = if visible { m.replace(off, on) } else { m.replace(on, off) };
        }
        self.run_now_if_ready(&format!("window.__adaChecker && window.__adaChecker.setMarkersVisible({visible});"));
    }

    /// Findings whose badge was clicked in the page since the last call.
    pub fn take_clicks(&self) -> Vec<(usize, usize)> {
        std::mem::take(&mut *self.clicks.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Highlights the element of `finding` in the page with a caption.
    pub fn highlight(&mut self, finding: &Finding, criterion: &str) {
        let (label, color) = severity_label(finding, criterion);
        let script = format!(
            "{HIGHLIGHT_JS}\nwindow.__adaChecker.show({}, {}, {});",
            locator_js(finding.locator.as_ref()),
            js_str(&label),
            js_str(color)
        );
        self.run_now_if_ready(&script);
        // Stored so it is (re)applied once the current navigation finishes.
        self.highlight = Some(script);
    }

    pub fn clear_highlight(&mut self) {
        self.highlight = None;
        if let Some(wv) = &self.webview {
            let _ = wv.evaluate_script("window.__adaChecker && window.__adaChecker.clear();");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_string_escaping() {
        assert_eq!(js_str(r#"a"b\c"#), r#""a\"b\\c""#);
        let closing = js_str("</script>");
        assert!(!closing.contains('<') && closing.contains("u003c"), "{closing}");
        assert_eq!(js_str("a\nb"), "\"a\\nb\"");
        assert!(js_str("x\u{2028}").ends_with("\\u2028\""));
    }

    #[test]
    fn ipc_messages_are_validated() {
        assert_eq!(parse_select("select:3:12"), Some((3, 12)));
        for bad in ["select:3", "select:a:1", "select:-1:2", "hello", "select:1:2:3", ""] {
            assert_eq!(parse_select(bad), None, "{bad}");
        }
    }

    #[test]
    fn locator_serialization() {
        let doc = scraper::Html::parse_document("<p>");
        let node = doc.root_element().id();
        let loc = Locator { tag: "img".into(), id: None, path: "html > body:nth-of-type(1)".into(), attr: Some(("src".into(), "a\".png".into())), node, source: None };
        assert_eq!(
            locator_js(Some(&loc)),
            r#"{tag: "img", id: null, path: "html > body:nth-of-type(1)", attr: ["src", "a\".png"]}"#
        );
        assert_eq!(locator_js(None), "null");
    }
}
