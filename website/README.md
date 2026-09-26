# Website

The single page that explains Meterix and links to the code. It is plain HTML,
one stylesheet and two self-hosted fonts. There is no build step, no package
manager and no third-party request of any kind.

Open `index.html` in a browser. That is the whole workflow. To serve it the way
a host would:

```
python -m http.server 8000
```

## Before this goes public

Three things are placeholders, and the page is honest about all of them.

**The origin.** `example.com` stands in for the real hostname in five places in
`index.html` (the canonical link, `og:url`, `og:image`, `twitter:image` and the
JSON-LD `url`), and in `robots.txt` and `sitemap.xml`. The reserved documentation
hostname was chosen on purpose: a forgotten one cannot point at somebody else's
site.

**The repository link.** The repository is not published yet, so the "Get it"
section says so rather than linking somewhere. When it exists, replace that
sentence with:

```html
<a class="repo-link" href="THE URL">Source on GitHub</a>
```

The style is already in the stylesheet.

**The social card.** `og:image` points at `og.png`, a 1200x630 PNG that does not
exist. Until it does, shared links show a text-only card. This is the one asset
the page still needs.

## Two decisions worth not undoing

**There are no photographs.** A tray utility's subject is its own interface, so
the previews are the app's real markup at the app's real dimensions, drawn with
the app's tokens. They cannot drift from the product the way a screenshot would,
and a stock photo of a laptop would say nothing true about it.

**The page is dark only, because the app is.** A light landing page wrapped
around a dark product would be a different brand. This contradicts the usual
rule that a consumer page ships both themes, and it is deliberate.

## Where the numbers come from

Every figure on the page is measured rather than invented: process size, idle
CPU, database growth and binary size come from the footprint records in
`docs/PROJECT.md`, taken on one machine and labelled that way. The provider
descriptions come from the API audit in the same document. If a number changes
there, change it here.

## Shared identity

The colour tokens in `styles.css` are copied from the app's own stylesheet
(`app/src/index.css`), including the four-step radius scale and the two fonts.
The page and the product are meant to read as one object at two distances, so a
token should move in both files or neither.

`--ink-muted` is used here only for large or non-essential type. Measured against
the backdrop it reaches 4.21:1, which is short of the 4.5:1 needed for body
copy, so body text uses `--ink-dim` at 7.63:1.
