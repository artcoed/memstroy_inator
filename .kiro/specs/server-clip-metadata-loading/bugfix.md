# Bugfix Requirements Document

## Introduction

The memstroy-assets-server has been deployed to Railway and the client has been configured to point to it. However, clips that exist on the server but haven't been downloaded locally don't appear in the library at all. This breaks the expected workflow where users should see all server-saved clips on app launch (with metadata and thumbnails loading incrementally), and only download the full video when dragging a clip to the canvas.

The root cause is that `reload_library()` in `state.rs` only scans the local filesystem for clips. There is no code path that fetches clip metadata from the server's `/api/assets?kind=clip&limit=50` endpoint during library initialization or refresh.

## Bug Analysis

### Current Behavior (Defect)

1.1 WHEN the app launches THEN the system only loads clips that exist in the local `~/.memstroy/cache/clips/` directory

1.2 WHEN `reload_library()` is called THEN the system only scans local `.txt`, `.jpg/.png` (thumbnails), and `.mp4` files without fetching from the server

1.3 WHEN clips exist on the server but not locally THEN the system does not display them in the library at all

1.4 WHEN the user manually triggers "Refresh" via `spawn_refresh()` THEN the system downloads metadata for clips, but this is a manual action not triggered on startup

### Expected Behavior (Correct)

2.1 WHEN the app launches THEN the system SHALL fetch clip metadata from the server's `/api/assets?kind=clip&limit=50` endpoint

2.2 WHEN `reload_library()` is called THEN the system SHALL merge local clips with server clips, showing both in the library without duplicates

2.3 WHEN clips exist on the server but not locally THEN the system SHALL display them in the library with their metadata (title/description) and thumbnails

2.4 WHEN the library loads THEN the system SHALL load metadata incrementally (50 clips at a time, more on scroll) to avoid overwhelming the UI

2.5 WHEN a clip has both local and server metadata THEN the system SHALL prioritize the local cache to avoid redundant network requests

2.6 WHEN the user drags a server-only clip to the canvas THEN the system SHALL download the full video on-demand using the existing `spawn_lazy_download()` functionality

### Unchanged Behavior (Regression Prevention)

3.1 WHEN clips exist only locally (legacy clips without server metadata) THEN the system SHALL CONTINUE TO display them in the library as before

3.2 WHEN the user manually triggers "Refresh" THEN the system SHALL CONTINUE TO download clips from Telegram via the server as before

3.3 WHEN a clip is already downloaded locally THEN the system SHALL CONTINUE TO use the local cached file instead of re-downloading

3.4 WHEN the user drags a downloaded clip to the canvas THEN the system SHALL CONTINUE TO use the local file immediately without network requests

3.5 WHEN the library rescans due to filesystem changes THEN the system SHALL CONTINUE TO pick up new local files as before
