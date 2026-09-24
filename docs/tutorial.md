# Tutorial

This walks through a session from an empty library to exported BibTeX.
Every command works from any directory.

## 1. Add papers

**By DOI.** ferref looks the paper up on Crossref and builds a cite key from the
first author's surname and the year.

```console
$ ferref add --doi 10.1103/PhysRev.106.620
Information Theory and Statistical Mechanics [jaynes1957]
  Type: article
  Authors: Jaynes, E. T.
  Year: 1957
  Journal: Physical Review
  Volume: 106
  Pages: 620-630
  DOI: 10.1103/PhysRev.106.620
```

Any other flag you pass (`--journal`, `--year`, `--abstract`, `--author`, and so
on) overrides what Crossref returned. `--key` sets your own cite key.

**By hand.** Give a type, a cite key, and a title. Repeat `--author` once per
author, written `"Last, First"`.

```sh
ferref add --type article --key shannon1948 \
  --title "A Mathematical Theory of Communication" \
  --author "Shannon, Claude E." \
  --year 1948 --journal "Bell System Technical Journal"
```

**From a web page.** Give `--url` on its own and ferref reads the page's
citation metadata (the `citation_*` tags publishers add for Google Scholar),
then downloads the PDF the page links to.

```console
$ ferref add --url https://arxiv.org/abs/1706.03762
Attention Is All You Need [vaswani2017]
  ...
Attached '~/.ferref/pdfs/vaswani2017.pdf'
```

This is the way to add arXiv papers, and papers your institution gives you
access to. Section 5 explains how the download works.

If you pass `--url` together with `--type`, `--key`, and `--title`, it is just
stored as the entry's URL and no page is fetched.

## 2. Look at your library

```console
$ ferref list
jaynes1957      1957   Information Theory and Statistical Mechanics       Jaynes, E. T.
shannon1948     1948   A Mathematical Theory of Communication             Shannon, Claude E.
```

`ferref show jaynes1957` prints one entry in full. Add `--json` to either
command for machine-readable output.

To change an entry, pass only the fields you want to change:

```sh
ferref edit shannon1948 --volume 27 --pages 379-423
```

## 3. Tags and collections

ferref has both, because they do different jobs:

- A **tag** describes a paper. A paper can have many tags, and tags don't nest.
- A **collection** is a folder a paper is filed in. Collections nest.

**Tags** are lowercased and trimmed, so `ML`, `ml`, and ` ml ` are the same tag.
Adding a tag twice does nothing.

```console
$ ferref tag jaynes1957 "  Entropy  "
Tagged 'jaynes1957' with 'entropy'
$ ferref tag jaynes1957 entropy
'jaynes1957' already tagged 'entropy'
```

**Collections** are written as paths. Creating a nested path creates every
level, like `mkdir -p`.

```console
$ ferref collection new "Information Theory/Foundations"
Created collection 'Information Theory/Foundations' (id 2)
$ ferref collection add "Information Theory/Foundations" shannon1948
Added 'shannon1948' to 'Information Theory/Foundations'
$ ferref collection add "Information Theory" jaynes1957
Added 'jaynes1957' to 'Information Theory'
$ ferref collection ls
Information Theory (1)
  Foundations (1)
```

Filtering by collection shows only papers filed directly in it. Add
`--recursive` to include everything underneath.

```console
$ ferref list --collection "Information Theory"
jaynes1957      1957   Information Theory and Statistical Mechanics       Jaynes, E. T.
$ ferref list --collection "Information Theory" --recursive
jaynes1957      1957   Information Theory and Statistical Mechanics       Jaynes, E. T.
shannon1948     1948   A Mathematical Theory of Communication             Shannon, Claude E.
```

`collection mv` moves a collection (and everything under it) to a new parent.
`collection delete` removes a collection and its subcollections, but never the
papers in them.

## 4. Search

Filters combine: a paper must match all of them.

```sh
ferref search --author jaynes
ferref search --title "information theory" --from 1950 --to 1960
ferref search --tag entropy --year 1957
```

`--author` and `--title` match any part of the text, ignoring case. `--tag`
must match the whole tag.

`--text` searches inside the papers themselves, and shows where each match
occurs:

```console
$ ferref search --text "scaled dot-product"
vaswani2017    Attention Is All You Need (2017)
  …Noam proposed scaled dot-product attention, multi-head attention and the parameter…
  …ors. The output is computed as a weighted sum 3 Scaled Dot-Product Attention…
  (+3 more matches in vaswani2017.pdf)
```

A search with no matches prints nothing and succeeds.

## 5. PDFs

Every PDF lives in `~/.ferref/pdfs/`, named after its cite key. However a paper
arrived, it ends up in that one folder, so backing up your library means
copying `~/.ferref`.

### Attach a file you already have

ferref copies the file into the library; your original stays where it is.
`--extract` converts it to text straight away.

```console
$ ferref attach jaynes1957 ~/Downloads/jaynes.pdf --extract
Attached '~/.ferref/pdfs/jaynes1957.pdf' to 'jaynes1957'
Extracted ... characters from '~/.ferref/pdfs/jaynes1957.pdf'
```

Attaching a second, different file to the same paper (a supplement, say)
stores it as `jaynes1957-2.pdf`. To extract text later, run
`ferref extract jaynes1957`. To read the paper, run `ferref open jaynes1957`.

### Fetch an open-access copy

`fetch` looks for a legal, free copy of a paper using its DOI. It needs a
contact email for Unpaywall ({doc}`installation` shows how to set one).

```console
$ ferref add --doi 10.1038/s41586-020-2649-2
Array programming with NumPy [harris2020]
  ...
$ ferref fetch harris2020
Downloaded open-access PDF for 'harris2020' from Unpaywall to '~/.ferref/pdfs/harris2020.pdf'
Extracted 41013 characters from '~/.ferref/pdfs/harris2020.pdf'
```

ferref asks Unpaywall first. If Unpaywall has nothing, it tries the preprint
server the DOI belongs to: arXiv, bioRxiv/medRxiv, OSF (including PsyArXiv,
SocArXiv, and others), or preprints.org.

Finding nothing is a normal result, not an error:

```console
$ ferref fetch piwowar2018
'piwowar2018' (DOI 10.7717/peerj.4375) is open access, but no direct PDF link
was found (tried: Unpaywall, arXiv, bioRxiv, OSF, preprints.org)
```

ferref never works around a paywall.

### Download from a page you can already read

`fetch` asks whether a paper is *free*. That's the wrong question for a paper
your university pays for. `ferref add --url <page>` asks a different one: it
requests the page the same way your browser would, from your network, and
keeps the PDF the page offers.

On a campus network or VPN, or through an `HTTPS_PROXY`, you get what your
browser gets. Elsewhere you get the metadata and a clear refusal:

```console
$ ferref add --url https://www.nature.com/articles/nature14539
Deep learning [lecun2015]
  ...
Warning: failed to download PDF: downloaded content is not a PDF (missing %PDF
magic bytes) -- this is usually an HTML interstitial, not the paper
```

The entry is saved either way. When the page lists a DOI, the metadata comes
from Crossref instead of the page, because Crossref is more reliable.

Results from outside any institutional network:

| Site | Metadata | PDF |
| --- | --- | --- |
| arXiv | yes | yes |
| PLOS | yes | yes |
| BioMed Central | yes | yes |
| Nature (paywalled) | yes | no: you get the paywall page, as expected |
| Wiley | no: blocks non-browser requests | no |
| science.org | no: the page has no citation tags | no |

## 6. Get the text back out

This is what ferref is for. `show --json` includes each attachment's text:

```console
$ ferref show harris2020 --json | jq -r '.attachments[].full_text' | head -3
Review

Array programming with NumPy
```

`list` and `search` leave the text out unless you pass `--full-text`, so that
an ordinary listing doesn't load every PDF into memory:

```sh
ferref list --json --full-text | jq -r '.[] | [.cite_key, (.attachments[0].full_text // "" | length)] | @tsv'
```

{doc}`scripting` has more recipes.

## 7. BibTeX

ferref doesn't format citations. It hands your LaTeX document a `.bib` file and
lets BibTeX or biblatex do the formatting.

```sh
ferref export > library.bib                          # the whole library
ferref export --collection "Information Theory" --recursive --out it.bib
ferref import colleague.bib                          # papers you already have are skipped
```

Tags are written to the `keywords` field and read back from it, so they survive
an export and re-import. Collections do not.

If your document uses the `biblatex` package, add `--biblatex`. It keeps entry
types like `@online` and `@dataset`, which plain BibTeX turns into `@misc`.

## 8. Keep the library tidy

**Missing files.** If you delete a PDF by hand, ferref still remembers it.
`doctor` lists attachments whose file is gone, and `doctor --fix` removes those
records:

```console
$ ferref doctor
1 of 3 attachments do not resolve on disk:
  shannon1948: ~/.ferref/pdfs/shannon1948.pdf
$ ferref doctor --fix
Fixed 1 of 1 broken attachments.
  shannon1948: ~/.ferref/pdfs/shannon1948.pdf
```

`ferref open` does the same cleanup for one paper when it finds a missing file,
and attaching or fetching a new PDF clears out that paper's missing ones first.

**Duplicates.** If the same paper was added twice, fold one into the other.
Tags, collections, and PDFs move to the entry you keep; its own fields are left
as they are.

```console
$ ferref merge shannon1948 shannon1948b
Merged 'shannon1948b' into 'shannon1948', deleting 'shannon1948b'
```
