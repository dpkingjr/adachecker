# ADA Checker

A Rust desktop app (egui/eframe) with two tools. The **Accessibility Checker** scans a web page for WCAG 2.0, 2.1 or 2.2 compliance at Level A, AA or AAA. The **Link Checker** checks a pasted list of links and reports which ones are broken.

## Run

```sh
cargo run --release
```

## Build the macOS app

```sh
scripts/bundle-macos.sh          # builds target/release/bundle/ADA Checker.app
scripts/bundle-macos.sh --dmg    # also builds ADA-Checker-<version>.dmg with an Applications shortcut
open "target/release/bundle/ADA Checker.app"
```

The script builds a release binary for macOS 11 and later. It also:

- puts the binary in a `.app` bundle with an `Info.plist` and an icon (`macos/make_icon.py` generates the icon, and `iconutil` turns it into an `.icns` file)
- ad-hoc code-signs the bundle

To install the app, drag it to `/Applications`. The bundle is built for the architecture of the Mac you build it on. It is not notarized, so if you share it with other Macs you need to sign it with a Developer ID and notarize it.

## Using the app

Enter a URL (the scheme is optional and defaults to `https://`) or an absolute local `.html` file path. Pick a WCAG version and level, then press **Scan** or Enter.

## Page preview

The right-hand panel shows the page in an embedded browser: WKWebView on macOS, through [`wry`](https://github.com/tauri-apps/wry). It starts loading as soon as you press **Scan**. Click a finding in the results list, or focus it with Tab and press Space, and the preview:

- scrolls to the element and outlines it in red (error) or amber (warning), with the criterion and message as a caption
- dims the rest of the page

Page-level findings, such as a missing `lang` attribute, are shown as a banner at the top of the preview.

The scanner records three locators for each element: its `id`, a CSS `nth-of-type` path, and a distinctive attribute such as `src` or `href`. They are tried in that order, because scripts can change the live page so it no longer matches the HTML that was scanned. If none of them finds the element, the preview says so.

To hide the panel, untick **Preview**. Drag the panel's left edge to resize it.

### HTML source view

The **HTML source** tab shows the exact HTML that was scanned, with syntax colouring and line numbers. Long lines, as in minified pages, wrap to the panel width. When you select a finding, the view scrolls to the element's start tag and highlights it in red or amber. The highlight also shows in the page preview. **Jump to selection** scrolls back to it.

html5ever doesn't record where each element came from in the source. To find it, a small lexer (`src/source.rs`) records the byte span of every start tag. Parsed elements are then matched to those tags in document order, by tag name and attributes. Elements the parser creates on its own, such as an implied `<tbody>` or a missing `<body>`, have no source tag, and the view says so. On six real sites (GitHub, W3C WAI, GOV.UK, Wikipedia, BBC News, MDN), all 9,189 elements matched their own start tags.

### Jumping between views

With **Show all issues** ticked, every flagged element is marked in both views:

- **Page preview:** a dashed outline and a badge (✖ for errors, ⚠ for warnings) showing how many issues the element has. Hover over the badge to read them.
- **HTML source:** the tag is underlined, and a ● appears in the gutter. Hover over the row to read the issues.

Click a mark in either view to jump to the element in the other one:

- **Badge in the page:** opens **HTML source**, scrolled to the element's tag.
- **Mark in the source:** opens **Page preview**, scrolled to the element and highlighted.

Either way, the results list expands the finding's rule and scrolls to the finding.

Badge clicks reach the app through wry's IPC channel as `select:<rule>:<finding>`. Scripts on the page can post to that channel too, so the app only accepts well-formed messages that refer to a real finding.

You can also pass a URL on the command line to scan it when the app opens: `adachecker https://example.com`.

## Link Checker

Select **Link Checker** in the left-hand navigation. Paste a list of links into the left pane and press **Check N links** at the top of the pane. The results table on the right fills in as each link is checked. It shows:

- the result
- the HTTP status code, for example `200 OK` or `404 Not Found`
- the redirect chain, when there is one
- the response time

What you can paste:

- One link per line. Links without `https://`, like `example.org/page`, also work.
- Links inside text, Markdown or HTML. The app picks them out.
- Lines starting with `#`, which are ignored as comments.
- Duplicate links, which are checked only once.

| Result | Meaning |
|---|---|
| ✔ OK | 2xx response |
| ↪ Redirected | Reached a 2xx response after one or more redirects. Details shows the chain, for example `301 → 200 → https://…` |
| ⚠ Restricted | 401, 403, 407 or 429. The server refused an automated request, but the link may still work in a browser. |
| ✖ Broken | Any other 4xx or 5xx response |
| ✖ Error | No HTTP response, for example a DNS failure, a refused connection, a TLS error or a timeout |

How links are checked:

- Eight links are checked at a time, and **Stop** cancels the run.
- Each link gets a `HEAD` request first, because it doesn't download the page. If that fails, the link is checked again with `GET` before it's reported as broken. Many servers mishandle `HEAD`.
- Redirects are followed by the app itself, up to 10, so the whole chain can be shown.
- **Show only problems** filters the table.
- **Copy as CSV** copies the results for a spreadsheet.

## What it checks

You can scan against **WCAG 2.0, 2.1, or 2.2** at Level **A, AA, or AAA**. The app defaults to WCAG 2.1 AA, the standard the ADA Title II rule refers to. Each rule is tagged with the WCAG version that introduced its success criterion. A scan runs only the rules that exist in the version you choose. Results for criteria added after 2.0 are labelled "new in 2.1" or "new in 2.2".

There are 49 rules. Most cover WCAG 2.0 criteria. The newer ones are:

- **Added in 2.1:** 1.3.4 Orientation, 1.3.5 Identify Input Purpose, 1.3.6 Identify Purpose, 1.4.10 Reflow, 1.4.11 Non-text Contrast, 1.4.12 Text Spacing, 1.4.13 Content on Hover or Focus, 2.1.4 Character Key Shortcuts, 2.3.3 Animation from Interactions, 2.5.1 Pointer Gestures, 2.5.2 Pointer Cancellation, 2.5.3 Label in Name, 2.5.4 Motion Actuation, 2.5.5 Target Size, and 4.1.3 Status Messages.
- **Added in 2.2:** 2.4.11 Focus Not Obscured, 2.5.8 Target Size (Minimum), and 3.3.8 Accessible Authentication.
- **4.1.1 Parsing:** in 2.0 and 2.1, duplicate `id` values fail this criterion. WCAG 2.2 removed it, so in a 2.2 scan duplicates are reported as a best practice. They are errors only when a label or ARIA attribute refers to the duplicated `id`.

The app has no automated check for these 2.1 and 2.2 criteria, so they still need manual testing: 2.2.6 Timeouts, 2.5.6 Concurrent Input Mechanisms, 2.4.12 and 2.4.13, 2.5.7 Dragging Movements, 3.2.6 Consistent Help, 3.3.7 Redundant Entry, and 3.3.9.

Each result is one of:

- **FAIL**: at least one definite error
- **REVIEW**: only warnings that a person needs to confirm
- **PASS**: no issues detected

Rules are defined in the `RULES` table in `src/checks.rs`. To add a check, write a `fn(&Ctx) -> Vec<Finding>` and add it to that table.

## Limitations

- The app analyzes the HTML the server returns. It does not run JavaScript, so content rendered on the client is not checked.
- Color contrast is checked only for inline `style` colors. Colors set in external stylesheets are not resolved.
- Automated tools catch only some WCAG failures. Manual testing with a keyboard and a screen reader is still needed.

## Tests

```sh
cargo test                       # unit tests
SCAN_FILE=page.html cargo test alignment_coverage -- --ignored --nocapture   # DOM-to-source mapping on a saved page
SCAN_URL=https://example.com cargo test -- --ignored --nocapture   # live scan, WCAG 2.1 AA
```
