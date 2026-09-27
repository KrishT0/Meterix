# The Meterix landing page

One HTML file and three fonts. No build step, no package manager, no third-party
request: the only off-origin strings in `index.html` are the `schema.org` and
`w3.org` vocabulary identifiers, which are not fetched.

```
index.html            the page: markup, stylesheet, and one script
404.html              served with a real 404 for anything that is not an asset
fonts/                three self-hosted variable fonts, copied from the app's own packages
og.png                1200x630 share card
apple-touch-icon.png  180x180, because iOS ignores an SVG favicon
robots.txt            allow everything, point at the sitemap
sitemap.xml           one URL
```

Open `index.html` from the file system to preview it.

## Deploying to Cloudflare Workers

`wrangler.jsonc` at the repository root points at this directory:

```
npx wrangler deploy
```

There is no Worker script. The site is files, so nothing runs per request, and
Cloudflare serves, caches and compresses them. `not_found_handling: "404-page"`
means an unmatched path returns `404.html` with a real 404 status rather than a
200 with "not found" written on it, which is the difference between a missing page
and a soft 404 that a crawler counts as thin content.

**One thing to settle before the first deploy, marked in the code:**

* **The origin.** Five places in `index.html` (`canonical`, `og:url`, `og:image`,
  `twitter:image`, the JSON-LD `url`), plus `robots.txt` and `sitemap.xml`, still
  use the reserved `example.com`. It is the one hostname that exists only for
  documentation, so a forgotten one cannot point at somebody else's site. Replace
  all of them with the real origin, and uncomment `routes` in `wrangler.jsonc` to
  attach a custom domain. A `workers.dev` subdomain works, but it is not the
  address that should end up in a canonical tag.

`og.png` is done, rendered from `mockup/og-card.html`. If a screenshot of the app is
wanted there instead, replace the file and keep it 1200x630.

## Cutting a release touches the site

The download section links to files by name, so a new release needs four edits in
`index.html` and nothing else. There is a comment in the section saying this too:

1. The version in the first paragraph ("Meterix 0.1.0, pre-alpha").
2. The tag in each of the seven download URLs (`releases/download/v0.1.0/...`).
3. The seven file names.
4. Their sizes.

The download **button** needs none of that. It ships as a plain link to the
releases page and is upgraded at runtime by a few lines of script, so it always
points at the newest published release on its own. That script is the only reason
the page talks to anything off-origin: it reads `api.github.com` in the visitor's
own browser, so the rate limit is theirs rather than a shared one.

### Response headers, honestly

`_headers` and `_redirects` **do** work here, and this directory ships neither yet.
The Workers static assets documentation has pages for both
(`/workers/static-assets/headers/` and `.../redirects/`), and the platform limits
page lists what they allow: 100 header rules, 2,000 static and 100 dynamic
redirects. An earlier version of this file claimed otherwise, having looked at the
wrong paths and generalised from two 404s.

So security or cache headers (HSTS, `X-Content-Type-Options`, a long
`Cache-Control` on `fonts/`) can be a plain text file in this directory when they
are wanted, with no Worker involved. Caching and compression of assets are already
automatic.

**One constraint to remember when that file lands:** the download script fetches
`https://api.github.com`, so any `Content-Security-Policy` must allow that host in
`connect-src`. Without it the button silently falls back to linking the releases
page, which works but is not the intent.

## Rebuilding the two images

Both are rendered in Chrome from card files, so they carry the same fonts and
tokens as the page and can be regenerated when the design changes. Nothing is
hand-drawn and nothing is exported from a design tool.

**The two card files are in `mockup/`, which is not in the repository** (see
`.gitignore`), so regenerating the images on another machine needs those two files
to come along with this directory. If they should be versioned instead, they want
a home outside `mockup/`.

Both images **are** committed, so regenerating one produces a diff that has to be
committed with the page change that prompted it. That is deliberate: a share card
that only exists on one machine is a share card that goes missing.

```
# og.png, 1200x630
chrome --headless=new --hide-scrollbars --force-device-scale-factor=1 \
  --window-size=1200,630 --virtual-time-budget=4000 \
  --screenshot=website/og.png file:///<repo>/mockup/og-card.html

# apple-touch-icon.png, 180x180
chrome --headless=new --hide-scrollbars --force-device-scale-factor=1 \
  --window-size=180,180 --virtual-time-budget=4000 \
  --screenshot=website/apple-touch-icon.png file:///<repo>/mockup/touch-icon.html
```

## The page that was here before

The earlier landing page (a separate `styles.css`, a two-column hero, six
sections that were all the same grid) was moved out of the repository to
`Projects/meterix/website-backup/` rather than deleted, so nothing is lost if a
part of it is wanted back. This page replaced it.

## Deliberate, and worth not "fixing"

**No photographs.** A tray utility's subject is its own interface, so the previews
are the app's real markup at the app's real dimensions. A screenshot ages; markup
cannot drift. This is the one place the design skill's "no div-based fake product
UI" rule is knowingly stepped around, and the reason is above.

**Dark only.** The app is dark. A light landing page wrapped around a dark product
would be a different brand. This contradicts the skill's dual-mode mandate on
purpose.

**The hero is stacked, not split.** The copy comes first and the product window
sits under it at the full width of the page. An earlier version had the window in
a second column bleeding off the right of the screen, which does not survive a wide
monitor: it ran past the page's own right gutter, got sliced by the edge of the
display, and the dashboard inside stretched across a thousand pixels with its
figures stranded at the far right.

**Chart numbers are the app's demo readings**, the same ones the previews above
them use (`$25.51`, `$18.42`, `$7.09`). The four figures in "what it costs to
leave running" are real measurements from a release build.

## The script

About 4 KB at the end of the body, and no dependencies. It does three things:
draws the chart's four lines the first time the chart is seen, resolves the four
cost figures out of scrambled digits, and animates the FAQ accordion (a `details`
element cannot animate its own content, so the summary click is intercepted).

None of it is load-bearing. Without JavaScript every line is already drawn, every
figure is already its real value, and the accordion is a plain `details` element
that still opens. Under `prefers-reduced-motion: reduce` the chart is drawn
immediately and the accordion toggles instantly.

## Copy rules this page is held to

No em-dashes or en-dashes anywhere (verified: zero of either). No `<br>` in
headlines. No version labels, no scroll cues, no "trusted by" strip, no decorative
status dots, no locale or weather strips, no fake-precise numbers. The small
uppercase mono labels above headings are limited to three, and they are the names
of the three surfaces, which is the only place such a label says something the
heading does not.

## Licence

The project is dual licensed, `MIT OR Apache-2.0`, at the user's option: the two
texts are `LICENSE-MIT` and `LICENSE-APACHE` at the repository root, and
`Cargo.toml` declares `license = "MIT OR Apache-2.0"` for both crates. So the
page's "MIT or Apache-2.0" and the JSON-LD `license` field are both accurate, and
the claim is backed by files rather than by a sentence on a page.
