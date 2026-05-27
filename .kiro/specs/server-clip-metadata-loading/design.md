# Server Clip Metadata Loading Bugfix Design

## Overview

The memstroy-gui application currently only displays clips that exist in the local filesystem cache (`~/.memstroy/cache/clips/`). Clips that have been saved to the server but not yet downloaded locally are invisible in the library, breaking the expected workflow where users should see all server-saved clips on app launch and only download the full video when needed.

The bug manifests because `reload_library()` in `state.rs` only scans the local filesystem for `.txt`, `.jpg/.png` (thumbnails), and `.mp4` files. There is no code path that fetches clip metadata from the server's `/api/assets?kind=clip&limit=50` endpoint during library initialization.

The fix will add automatic server metadata fetching to `reload_library()`, merging server clips with local clips to provide a unified library view. Full video downloads will remain on-demand via the existing `spawn_lazy_download()` mechanism.

## Glossary

- **Bug_Condition (C)**: The condition that triggers the bug - when clips exist on the server but not in the local cache, and `reload_library()` is called
- **Property (P)**: The desired behavior - server clips should appear in the library with metadata and thumbnails, even if the video file hasn't been downloaded yet
- **Preservation**: Existing local-only clip loading, manual refresh workflow, and lazy download behavior that must remain unchanged by the fix
- **reload_library()**: The function in `crates/memstroy-gui/src/state.rs` that scans the local filesystem and populates `self.library.mellstroy_clips`
- **fetch_server_clips_metadata()**: The function (already exists) in `state.rs` that fetches clip metadata from the server's `/api/assets?kind=clip&limit=50` endpoint
- **spawn_lazy_download()**: The existing function in `jobs.rs` that downloads full video files on-demand when a clip is dragged to the canvas
- **LibraryClip**: The struct representing a clip in the library, with fields: `id`, `path`, `description`, `downloaded`, `thumbnail`, `server_id`
- **server_id**: The clip's identifier on the server (used to construct API URLs like `/api/assets/{id}/download`)

## Bug Details

### Bug Condition

The bug manifests when clips exist on the server but not in the local cache directory (`~/.memstroy/cache/clips/`). The `reload_library()` function only scans the local filesystem for `.txt` (metadata), `.jpg/.png` (thumbnails), and `.mp4` (video) files. It does not fetch clip metadata from the server's `/api/assets?kind=clip&limit=50` endpoint, so server-only clips never appear in the library.

**Formal Specification:**
```
FUNCTION isBugCondition(input)
  INPUT: input of type LibraryReloadContext
  OUTPUT: boolean
  
  RETURN input.server_has_clips == true
         AND input.clips_exist_on_server_but_not_locally == true
         AND input.reload_library_called == true
         AND NOT input.server_metadata_fetched_during_reload
END FUNCTION
```

### Examples

- **Example 1**: User launches the app after clips have been saved to the server from another device. Expected: All server clips appear in the library with metadata/thumbnails. Actual: Library is empty or only shows old local clips.

- **Example 2**: User manually triggers "Refresh" which downloads clips from Telegram to the server. Expected: After refresh completes, all clips appear in the library. Actual: Clips only appear after manually triggering another refresh or restarting the app.

- **Example 3**: User has 10 clips on the server and 5 clips in local cache. Expected: Library shows all 15 clips (5 marked as downloaded, 10 as server-only). Actual: Library only shows the 5 local clips.

- **Edge Case**: User has no server URL configured (`server_url` is empty). Expected: Library shows only local clips without errors. Actual: Same behavior (no server fetch attempted).

## Expected Behavior

### Preservation Requirements

**Unchanged Behaviors:**
- Local-only clips (legacy clips without server metadata) must continue to appear in the library as before
- Manual "Refresh" workflow (`spawn_refresh()`) must continue to download clips from Telegram via the server
- Lazy download workflow (`spawn_lazy_download()`) must continue to download full videos on-demand when clips are dragged to the canvas
- Clips that are already downloaded locally must continue to use the local cached file instead of re-downloading
- Filesystem change detection (`auto_rescan_local_library_if_due()`) must continue to pick up new local files

**Scope:**
All inputs that do NOT involve server-only clips should be completely unaffected by this fix. This includes:
- Local-only clips (clips with `.mp4` files but no `server_id`)
- Manual refresh operations (Telegram ingestion via `spawn_refresh()`)
- Lazy download operations (on-demand video download via `spawn_lazy_download()`)
- Filesystem monitoring (automatic rescan when local files change)

## Hypothesized Root Cause

Based on the bug description and code analysis, the root cause is:

1. **Missing Server Fetch in reload_library()**: The `reload_library()` function only scans the local filesystem (three passes: `.txt` files, thumbnails, `.mp4` files). It does not call `fetch_server_clips_metadata()` or any other function to fetch clip metadata from the server.

2. **fetch_server_clips_metadata() Already Exists**: The function `fetch_server_clips_metadata()` already exists in `state.rs` and correctly fetches clip metadata from `/api/assets?kind=clip&limit=50`, downloads `.txt` and thumbnail files for server-only clips, and sends a `JobEvent::RefreshLibraryReloaded` to trigger UI refresh. However, it is never called during library initialization.

3. **Manual Refresh Works**: The manual refresh workflow (`spawn_refresh()`) works correctly because it explicitly calls the server API to download clips from Telegram, which creates local metadata files that `reload_library()` can then pick up.

4. **Lazy Download Works**: The lazy download workflow (`spawn_lazy_download()`) works correctly because it is triggered when a clip is dragged to the canvas, independent of library loading.

## Correctness Properties

Property 1: Bug Condition - Server Clips Appear in Library

_For any_ library reload where clips exist on the server but not in the local cache, the fixed `reload_library()` function SHALL fetch clip metadata from the server's `/api/assets?kind=clip&limit=50` endpoint, download `.txt` and thumbnail files for server-only clips, and display them in the library with their metadata and thumbnails (marked as `downloaded: false`).

**Validates: Requirements 2.1, 2.2, 2.3, 2.4**

Property 2: Preservation - Local-Only Clips

_For any_ library reload where clips exist only locally (legacy clips without server metadata), the fixed `reload_library()` function SHALL produce the same result as the original function, preserving the ability to load and display local-only clips without server interaction.

**Validates: Requirements 3.1**

Property 3: Preservation - Manual Refresh Workflow

_For any_ manual refresh operation triggered by the user, the fixed code SHALL produce exactly the same behavior as the original code, preserving the Telegram ingestion workflow via `spawn_refresh()`.

**Validates: Requirements 3.2**

Property 4: Preservation - Lazy Download Workflow

_For any_ clip drag operation where the clip's video file is not downloaded locally, the fixed code SHALL produce exactly the same behavior as the original code, preserving the on-demand video download via `spawn_lazy_download()`.

**Validates: Requirements 3.3, 3.4**

Property 5: Preservation - Local Cache Priority

_For any_ clip that has both local metadata files AND server metadata, the fixed `reload_library()` function SHALL prioritize the local cache and NOT fetch from the server, preserving the existing behavior of using local files when they exist.

**Validates: Requirements 2.5**

Property 6: Preservation - Filesystem Change Detection

_For any_ filesystem change in the local clips directory, the fixed code SHALL produce exactly the same behavior as the original code, preserving the automatic rescan via `auto_rescan_local_library_if_due()`.

**Validates: Requirements 3.5**

## Fix Implementation

### Changes Required

Assuming our root cause analysis is correct (missing server fetch in `reload_library()`):

**File**: `crates/memstroy-gui/src/state.rs`

**Function**: `reload_library()`

**Specific Changes**:

1. **Verify fetch_server_clips_metadata() is called**: The function `fetch_server_clips_metadata()` already exists and is called at the end of `reload_library()` (line 2113 in the current code). This is correct. The issue is that `fetch_server_clips_metadata()` spawns an async task that downloads metadata files, but `reload_library()` doesn't wait for it to complete before returning.

2. **Root Cause Refinement**: The actual issue is that `fetch_server_clips_metadata()` downloads metadata files asynchronously, but `reload_library()` doesn't trigger a second scan after the metadata files are downloaded. The async task sends a `JobEvent::RefreshLibraryReloaded` event, but this event is not handled in `app.rs` to trigger a second `reload_library()` call.

3. **Add JobEvent Handler**: In `crates/memstroy-gui/src/app.rs`, add a handler for `JobEvent::RefreshLibraryReloaded` that calls `self.state.reload_library()` to pick up the newly downloaded metadata files.

4. **Verify Event is Sent**: Ensure that `fetch_server_clips_metadata()` sends the `JobEvent::RefreshLibraryReloaded` event after downloading metadata files. (This needs to be verified in the code.)

5. **Handle Graceful Degradation**: Ensure that if the server is unreachable, `fetch_server_clips_metadata()` logs a warning and continues with local-only clips (this is already implemented).

**Alternative Approach (if JobEvent handler doesn't exist)**:

If the `JobEvent::RefreshLibraryReloaded` event doesn't exist or isn't sent by `fetch_server_clips_metadata()`, we need to:

1. **Add JobEvent Variant**: Add `RefreshLibraryReloaded` to the `JobEvent` enum in `jobs.rs`
2. **Send Event After Metadata Download**: Modify `fetch_server_clips_metadata()` to send `JobEvent::RefreshLibraryReloaded` after downloading metadata files
3. **Handle Event in app.rs**: Add a handler in `app.rs` that calls `self.state.reload_library()` when `RefreshLibraryReloaded` is received

## Testing Strategy

### Validation Approach

The testing strategy follows a two-phase approach: first, surface counterexamples that demonstrate the bug on unfixed code, then verify the fix works correctly and preserves existing behavior.

### Exploratory Bug Condition Checking

**Goal**: Surface counterexamples that demonstrate the bug BEFORE implementing the fix. Confirm or refute the root cause analysis. If we refute, we will need to re-hypothesize.

**Test Plan**: Write tests that simulate a server with clips, mock the `/api/assets?kind=clip&limit=50` endpoint, and verify that `reload_library()` fetches metadata and displays server-only clips. Run these tests on the UNFIXED code to observe failures and understand the root cause.

**Test Cases**:
1. **Server-Only Clips Test**: Create a mock server with 3 clips, ensure local cache is empty, call `reload_library()`, verify that clips do NOT appear in the library (will fail on unfixed code - confirms bug)
2. **Mixed Local and Server Clips Test**: Create a mock server with 5 clips, create local metadata for 2 clips, call `reload_library()`, verify that only 2 local clips appear (will fail on unfixed code - confirms bug)
3. **Server Unreachable Test**: Disable server, create local clips, call `reload_library()`, verify that local clips still appear (should pass on unfixed code - confirms graceful degradation)
4. **No Server URL Test**: Set `server_url` to empty string, create local clips, call `reload_library()`, verify that local clips appear without errors (should pass on unfixed code - confirms graceful degradation)

**Expected Counterexamples**:
- Server-only clips do not appear in the library after `reload_library()` is called
- Possible causes: `fetch_server_clips_metadata()` is not called, or it is called but doesn't trigger a second library scan after metadata files are downloaded

### Fix Checking

**Goal**: Verify that for all inputs where the bug condition holds, the fixed function produces the expected behavior.

**Pseudocode:**
```
FOR ALL input WHERE isBugCondition(input) DO
  result := reload_library_fixed(input)
  ASSERT expectedBehavior(result)
END FOR
```

**Expected Behavior:**
- Server-only clips appear in the library with metadata and thumbnails
- Clips are marked as `downloaded: false`
- Full video download is deferred until the clip is dragged to the canvas

### Preservation Checking

**Goal**: Verify that for all inputs where the bug condition does NOT hold, the fixed function produces the same result as the original function.

**Pseudocode:**
```
FOR ALL input WHERE NOT isBugCondition(input) DO
  ASSERT reload_library_original(input) = reload_library_fixed(input)
END FOR
```

**Testing Approach**: Property-based testing is recommended for preservation checking because:
- It generates many test cases automatically across the input domain
- It catches edge cases that manual unit tests might miss
- It provides strong guarantees that behavior is unchanged for all non-buggy inputs

**Test Plan**: Observe behavior on UNFIXED code first for local-only clips, manual refresh, and lazy download, then write property-based tests capturing that behavior.

**Test Cases**:
1. **Local-Only Clips Preservation**: Create local clips with `.mp4`, `.txt`, and thumbnail files (no server metadata), call `reload_library()` on UNFIXED code, observe that clips appear correctly, then write test to verify this continues after fix
2. **Manual Refresh Preservation**: Trigger manual refresh via `spawn_refresh()` on UNFIXED code, observe that Telegram ingestion works correctly, then write test to verify this continues after fix
3. **Lazy Download Preservation**: Drag a server-only clip to the canvas on UNFIXED code, observe that `spawn_lazy_download()` downloads the video, then write test to verify this continues after fix
4. **Local Cache Priority Preservation**: Create a clip with both local and server metadata on UNFIXED code, observe that local files are used, then write test to verify this continues after fix
5. **Filesystem Change Detection Preservation**: Add a new local clip file on UNFIXED code, observe that `auto_rescan_local_library_if_due()` picks it up, then write test to verify this continues after fix

### Unit Tests

- Test `reload_library()` with empty local cache and mock server returning 3 clips
- Test `reload_library()` with 2 local clips and mock server returning 5 clips (3 new)
- Test `reload_library()` with server unreachable (should not crash, should show local clips)
- Test `reload_library()` with empty `server_url` (should not crash, should show local clips)
- Test that `fetch_server_clips_metadata()` sends `JobEvent::RefreshLibraryReloaded` after downloading metadata
- Test that `app.rs` handles `JobEvent::RefreshLibraryReloaded` by calling `reload_library()`

### Property-Based Tests

- Generate random combinations of local clips and server clips, verify that all clips appear in the library after `reload_library()`
- Generate random server responses (empty, partial, full), verify that library correctly merges with local clips
- Generate random filesystem states (clips with/without metadata, with/without thumbnails), verify that all valid clips are loaded
- Generate random server availability states (reachable/unreachable), verify that local clips always appear

### Integration Tests

- Test full app launch flow: start app, verify server metadata is fetched, verify clips appear in library
- Test manual refresh flow: trigger refresh, verify Telegram ingestion works, verify clips appear in library
- Test lazy download flow: drag server-only clip to canvas, verify video is downloaded, verify clip plays correctly
- Test mixed workflow: start app with some local clips, trigger refresh to add server clips, verify all clips appear
- Test offline workflow: start app with server unreachable, verify local clips appear, verify no errors
