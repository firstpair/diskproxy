# Quick start: mounting a camera card

This assumes your default config (`~/.config/diskproxy/config.json`) uses the PMP plugin
with `"source": "/Volumes/LEICA M"`, as installed by
`scripts/install.sh --config configs/pmp.json`.

1. Insert the card and wait until `/Volumes/LEICA M` mounts.
2. Run `diskproxy start`.
   - A card that hasn't been registered yet is registered first (it prints
     `New card: registering it with diskproxy-server...`), then its view starts.
   - So finish any EXIF corrections before starting a new card. If the Mac's
     timezone changed while the card stayed mounted, remount the card first, or
     register it yourself with the offset it was mounted in, for example
     `diskproxy-server --source-offset=+09:00`, before `diskproxy start`.
3. In Photo Mechanic Plus, pick the **PMP Pacific** volume under **Disks** and ingest
   from it, not from the card itself.
4. When you're done, run `diskproxy stop`, then eject the card.

## Extras

- `diskproxy status` shows whether it's running.
- To hide photos you've already ingested, use
  `diskproxy start --set plugins.pmp.hide_ingested=true`.
- If the card is named something other than `LEICA M`, pass its path:
  `diskproxy start "/Volumes/<card name>"`.
- Don't change files on the card while the view is mounted, or reads will fail.

Full guide: [plugins/pmp/README.md](plugins/pmp/README.md).
