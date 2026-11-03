slug: diskproxy-manual
shelf: technology, photography
default_edition: full

# FirstPair publication contract

## Ownership

This repository owns the manual in `docs/diskproxy-manual.md`, the project story in `docs/blog/diskproxy/post.md`, the unchanged supplied cover in `cover/diskproxy-cover.png`, `book.build.json`, `docs/book/`, and the built `dist/` package and version manifest. The source is public at <https://github.com/firstpair/diskproxy>. FirstPair owns the catalog entry, stable reader/download routes, upload manifests and delivery metadata. Follow the shared rules in `~/src/firstpair/AGENTS.md`.

The manual and the code are MIT licensed. The manual contains no personal paths, catalogs, photos or inventories; its measurements are stated as observations of one library and one camera card.

## Build

Commit and push all source changes before building a publication handoff, so the edition is stamped with the exact source commit. Verify the complete repository's clean, exact upstream state with the canonical preflight:

```sh
cd /absolute/path/to/diskproxy
uv run --no-project python "$HOME/src/firstpair/publishing/scripts/git_publish_preflight.py" .
"$HOME/src/firstpair/publishing/scripts/build-library-book.sh" --repo-root "$PWD" --print-plan
"$HOME/src/firstpair/publishing/scripts/build-library-book.sh" --repo-root "$PWD"
"$HOME/src/firstpair/publishing/scripts/verify-library-book.sh" "$PWD/dist"
```

The builder verifies the shared pinned toolchain and emits PDF, EPUB, self-contained HTML, split chapters and `dist/VERSION.md`. It uses the unchanged wide artwork with `contain`; no crop or image transformation is performed. The source-link filter points exported links at the exact source commit. Inspect the PDF pages with Poppler, including the cover, the information page, tables, commands and chapter transitions. Keep `build/` ignored. Commit and push the reviewed `dist/` artifacts and manifest before publication.

## Publish

This title's intended edition is **full**. Both this source repository and FirstPair must be clean and pushed before publication. Preserve unrelated FirstPair worktrees and changes. Resolve the plan before uploading:

```sh
cd "$HOME/src/firstpair"
npm run library:publish -- /absolute/path/to/diskproxy \
  --slug diskproxy-manual --full \
  --title "The diskproxy Manual" \
  --description "Virtual disks, NFS and WebDAV views that never write to the source: commands, configuration, writable views, camera-card ingest, preview libraries and plugins." \
  --kicker "A complete user and developer guide" --tags "macos,photography,filesystems,rust" \
  --dry-run --no-build --no-smoke --no-deploy --no-icloud
```

After the plan and artifacts are reviewed and publication is authorized, run the same command without the dry-run suppression flags. Commit and push the FirstPair metadata produced in that same publication session.

Reader-facing routes will be `https://firstpair.org/diskproxy-manual/pdf/`, `/diskproxy-manual/epub/`, `/read/diskproxy-manual/` and `/read/diskproxy-manual/chapters/`. Raw object-storage URLs belong in FirstPair manifests, not in reader-facing prose.

## Blog

The project story is `docs/blog/diskproxy/post.md`, published at `https://firstpair.press/diskproxy/`. After its source changes are committed and pushed, stamp its textpack, commit and push the three generated files, then deliver it:

```sh
cd /absolute/path/to/diskproxy
REPO_ROOT="$PWD" BLOG_DOMAIN=firstpair.press \
BLOG_TAGS=rust,photography,filesystems,nfs,webdav,plugins \
BLOG_EXCERPT="A Leica ingest failure became a Rust proxy with stable timestamps, streamed virtual disks, a photo library served from its previews, and ratings captured without touching a file." \
"$HOME/src/firstpair/publishing/scripts/stamp-versioned-blog.sh" docs/blog/diskproxy
REPO_ROOT="$PWD" BLOG_DOMAIN=firstpair.press \
"$HOME/src/firstpair/publishing/scripts/publish-versioned-blog.sh" docs/blog/diskproxy "$HOME/icloud/blogs"
```
