// =============================================================
// PORTRAIT CROPPER — a centered modal that lets the user crop a picked
// portrait to a 2:3 aspect before it lands in the Player Creator slot.
//
// FLOW: openPortraitCropper(parentEl, imageSrc) → loads the image into a
// fixed-aspect 2:3 stage, overlays a draggable + resizable crop rectangle
// (corner handles), + on Confirm draws the cropped region to an offscreen
// canvas + returns { dataUrl, bytes, ext } (PNG). On Cancel resolves null.
//
// GEOMETRY: the .fable-crop-stage is a fixed 2:3 box. The image fills it via
// object-fit: cover (so it always fills the stage regardless of source
// aspect). The crop rectangle lives in STAGE-pixel coordinates — so the crop
// maps 1:1 onto what's painted. On confirm we draw the crop rect's stage-pixel
// region straight to a 2:3 canvas at the source image's NATURAL resolution
// (scaled up from stage px → natural px), so the saved portrait keeps full
// fidelity (not the downscaled stage preview).
//
// OUTPUT: PNG (2026-08-19 — was JPEG 0.92). The launcher's `<Name>.ico`
// wraps the portrait's PNG bytes verbatim (ICO can't embed a JPEG — a JPG
// portrait meant the shortcut silently fell back to the F icon forever),
// and PNG is lossless. The fixed 480×720 output keeps the size modest.
// ext = "png". bytes = Uint8Array — callers send it over IPC as BASE64
// (bytesB64 over JSON, e.g. fable_player_portrait_upload_bytes). A bare
// byte-array IPC arg does NOT deserialize through Tauri v2's invoke and
// poisons command registration (AGENTS.md anti-pattern #5).
//
// The modal is appended to parentEl (the player-creator screen) + removed on
// resolve. Esc cancels. The overlay sits below the OS-level Wupi top bar
// (z:60, same tier as the raw editor) so it doesn't escape the Fable stage.
// =============================================================

const CROP_ASPECT = 2 / 3;   // width / height (portrait)
const MIN_CROP_W = 40;       // stage-px floor on crop width
const OUTPUT_W = 480;        // output canvas width (natural-scale target)
const OUTPUT_H = Math.round(OUTPUT_W / CROP_ASPECT); // 720

// (2026-08-16 yellow J7) The loaded image is PER-MODAL state (a local in
// openPortraitCropper), not a module global: the decode is async, and a
// module-global meant a Confirm that raced the new image's onload drew the
// PREVIOUS modal's image (cropping stale bytes). One in-flight modal at a
// time regardless (the creator is modal).

/**
 * Open the cropper modal against `parentEl` with `imageSrc` (a URL the
 * browser can paint — typically convertFileSrc of the picked path).
 * Resolves { dataUrl, bytes, ext } on Confirm, or null on Cancel / load error.
 */
export function openPortraitCropper(parentEl, imageSrc) {
  return new Promise((resolve) => {
    // Build the modal DOM (CSS lives in flow-cinematic.css).
    const overlay = document.createElement('div');
    overlay.className = 'fable-crop-overlay';
    overlay.innerHTML = `
      <div class="fable-crop-modal" role="dialog" aria-modal="true" aria-label="Crop portrait">
        <h2 class="fable-crop-title">Crop Portrait</h2>
        <div class="fable-crop-stage" data-crop-stage>
          <img class="fable-crop-img" data-crop-img alt="" draggable="false">
          <div class="fable-crop-mask" data-crop-mask></div>
          <div class="fable-crop-rect" data-crop-rect>
            <span class="fable-crop-handle" data-h="nw"></span>
            <span class="fable-crop-handle" data-h="ne"></span>
            <span class="fable-crop-handle" data-h="sw"></span>
            <span class="fable-crop-handle" data-h="se"></span>
          </div>
        </div>
        <div class="fable-crop-actions">
          <button type="button" class="fable-crop-btn" data-crop-cancel>Cancel</button>
          <button type="button" class="fable-crop-btn fable-crop-btn--confirm" data-crop-confirm>Confirm</button>
        </div>
      </div>
    `;
    parentEl.appendChild(overlay);
    // Force a reflow so the opacity transition plays on the next frame.
    void overlay.offsetWidth;
    overlay.classList.add('is-open');

    const img = overlay.querySelector('[data-crop-img]');
    const stage = overlay.querySelector('[data-crop-stage]');
    const rect = overlay.querySelector('[data-crop-rect]');
    const mask = overlay.querySelector('[data-crop-mask]');
    const confirmBtn = overlay.querySelector('[data-crop-confirm]');
    const cancelBtn = overlay.querySelector('[data-crop-cancel]');

    let settled = false;
    // (yellow J7) Per-modal decoded image — null until the probe's onload.
    // Confirm before the decode lands is a no-op (below), never a draw of a
    // previous modal's image.
    let loadedImg = null;
    // (2026-08-15 audit fix) document-level (capture) key handler — the old
    // overlay-bound listener went deaf once focus left the overlay (dragging
    // the crop rect moves focus to body). Removed on settle; the settled +
    // isConnected guards keep a late event from re-firing after close.
    let onDocKey = null;
    function done(result) {
      if (settled) return;
      settled = true;
      if (onDocKey) document.removeEventListener('keydown', onDocKey, { capture: true });
      overlay.classList.remove('is-open');
      // Wait for the fade-out transition before removing + resolving so the
      // modal doesn't snap out of existence.
      setTimeout(() => {
        overlay.remove();
        resolve(result);
      }, 200);
    }

    // Load the image. On error, surface + cancel (no portrait).
    const probe = new Image();
    probe.onload = () => {
      loadedImg = probe;
      img.src = imageSrc;
      // After the <img> paints (cover), size the crop rect to a centered
      // default that fits inside the stage. Defer one frame so layout settled.
      requestAnimationFrame(() => centerDefaultRect());
    };
    probe.onerror = () => {
      // Could not load — cancel silently (the slot keeps its prior state).
      done(null);
    };
    probe.src = imageSrc;

    // --- Crop-rect geometry (stage-pixel space) --------------------------
    // The rect is constrained to the stage bounds + kept at the 2:3 aspect.
    let rX = 0, rY = 0, rW = 0, rH = 0; // current rect (stage px)

    function applyRect() {
      rect.style.left = `${rX}px`;
      rect.style.top = `${rY}px`;
      rect.style.width = `${rW}px`;
      rect.style.height = `${rH}px`;
      // Carve the rect out of the mask so the underlying image shows through.
      // Use clip-path on the mask: the mask covers everything EXCEPT the rect.
      mask.style.clipPath = `polygon(
        0% 0%, 100% 0%, 100% 100%, 0% 100%, 0% 0%,
        ${rX}px ${rY}px,
        ${rX}px ${rY + rH}px,
        ${rX + rW}px ${rY + rH}px,
        ${rX + rW}px ${rY}px,
        ${rX}px ${rY}px
      )`;
    }

    function stageSize() {
      return { w: stage.clientWidth, h: stage.clientHeight };
    }

    function centerDefaultRect() {
      const { w, h } = stageSize();
      if (!w || !h) return;
      // MAX centered 2:3 fit: the largest 2:3 rect that fits inside the stage,
      // centered. A genuine portrait image can be confirmed with ZERO adjustment
      // (Chloe 2026-08-05: "maxed out centered so genuine portraits you can just
      // hit crop without doing any edits"). The user can still drag/resize down
      // via the corner handles if they want a tighter crop.
      // Height-driven: a 2:3 rect is width:height = 2:3. Fit by height first
      // (stage is itself 2:3, so height-fit ≈ stage-filling), then clamp by width.
      rH = h;
      rW = Math.max(MIN_CROP_W, Math.floor(rH * CROP_ASPECT));
      if (rW > w) {
        rW = w;
        rH = Math.floor(rW / CROP_ASPECT);
      }
      rX = Math.floor((w - rW) / 2);
      rY = Math.floor((h - rH) / 2);
      applyRect();
    }

    // Fit a 2:3 rect against an anchor corner. `ax, ay` is the stage-px
    // coordinate of the corner that must stay fixed (the non-dragged corner);
    // `anchor` names which corner it is. `w, h` are the requested dimensions
    // (h is authoritative — w is re-derived from the 2:3 aspect). Returns a
    // {x,y,w,h} rect positioned so the anchor corner is preserved, clamped to
    // the stage. Used by the resize handles.
    function clampRect(ax, ay, w, h, anchor) {
      const { w: sw, h: sh } = stageSize();
      // Height drives the portrait aspect; re-derive width from it.
      h = Math.max(MIN_CROP_W / CROP_ASPECT, h);
      w = Math.round(h * CROP_ASPECT);
      w = Math.max(MIN_CROP_W, w);
      // Cap to stage first so the anchor math doesn't overflow.
      if (h > sh) { h = sh; w = Math.round(h * CROP_ASPECT); }
      if (w > sw) { w = sw; h = Math.round(w / CROP_ASPECT); }
      // Position the rect so the named anchor corner sits at (ax, ay).
      let nx = ax, ny = ay;
      if (anchor === 'nw') { nx = ax; ny = ay; }
      else if (anchor === 'ne') { nx = ax - w; ny = ay; }
      else if (anchor === 'sw') { nx = ax; ny = ay - h; }
      else if (anchor === 'se') { nx = ax - w; ny = ay - h; }
      // Clamp into the stage without breaking the anchor (slide, don't resize).
      if (nx < 0) nx = 0;
      if (ny < 0) ny = 0;
      if (nx + w > sw) nx = sw - w;
      if (ny + h > sh) ny = sh - h;
      return { x: nx, y: ny, w, h };
    }

    // --- Pointer interaction (drag to move, handles to resize) -----------
    let drag = null; // { mode: 'move'|'nw'|'ne'|'sw'|'se', startX, startY, orig }

    function onPointerDown(e) {
      const handle = e.target.closest('[data-h]');
      const mode = handle ? handle.dataset.h : 'move';
      drag = { mode, startX: e.clientX, startY: e.clientY, orig: { x: rX, y: rY, w: rW, h: rH } };
      // Capture so the pointer keeps feeding events even outside the rect.
      try { e.currentTarget.setPointerCapture(e.pointerId); } catch (_) {}
      e.preventDefault();
    }
    function onPointerMove(e) {
      if (!drag) return;
      const dx = e.clientX - drag.startX;
      const dy = e.clientY - drag.startY;
      const o = drag.orig;
      if (drag.mode === 'move') {
        const { w, h } = stageSize();
        let nx = o.x + dx;
        let ny = o.y + dy;
        nx = Math.max(0, Math.min(nx, w - o.w));
        ny = Math.max(0, Math.min(ny, h - o.h));
        rX = nx; rY = ny;
      } else {
        // Resize from a corner. The dragged corner follows the pointer; the
        // opposite (anchored) corner stays fixed. Track the drag as a (left,
        // top, right, bottom) edge box, then re-fit a 2:3 rect anchored on the
        // non-dragged corner. `anchor` names which corner is fixed (the one
        // opposite the handle being dragged).
        let left = o.x, top = o.y, right = o.x + o.w, bottom = o.y + o.h;
        if (drag.mode === 'se') { right += dx; bottom += dy; }
        else if (drag.mode === 'nw') { left += dx; top += dy; }
        else if (drag.mode === 'ne') { right += dx; top += dy; }
        else if (drag.mode === 'sw') { left += dx; bottom += dy; }
        const boxW = Math.max(MIN_CROP_W, right - left);
        const boxH = Math.max(MIN_CROP_W / CROP_ASPECT, bottom - top);
        // The anchored corner is the OPPOSITE of the dragged handle.
        let anchorName, anchorX, anchorY;
        if (drag.mode === 'se') { anchorName = 'nw'; anchorX = left; anchorY = top; }
        else if (drag.mode === 'nw') { anchorName = 'se'; anchorX = right; anchorY = bottom; }
        else if (drag.mode === 'ne') { anchorName = 'sw'; anchorX = left; anchorY = bottom; }
        else /* sw */ { anchorName = 'ne'; anchorX = right; anchorY = top; }
        const next = clampRect(anchorX, anchorY, boxW, boxH, anchorName);
        rX = next.x; rY = next.y; rW = next.w; rH = next.h;
      }
      applyRect();
    }
    function onPointerUp(e) {
      drag = null;
      try { e.currentTarget.releasePointerCapture(e.pointerId); } catch (_) {}
    }

    rect.addEventListener('pointerdown', onPointerDown);
    rect.addEventListener('pointermove', onPointerMove);
    rect.addEventListener('pointerup', onPointerUp);
    rect.addEventListener('pointercancel', onPointerUp);

    // --- Confirm: draw the cropped region to an offscreen canvas ---------
    // Wrapped in try/catch (Chloe 2026-08-05): the prior version had NO guard,
    // so a tainted-canvas SecurityError from drawImage/toDataURL (caused by
    // feeding the cropper a cross-origin convertFileSrc `asset://` URL) escaped
    // + done() never fired → the modal hung on Confirm ("press crop, nothing
    // happens"). The real fix is feeding the cropper a same-origin data URL
    // (player-creator.js now reads the picked bytes server-side); this try/catch
    // is the belt-and-suspenders so any future failure surfaces + cancels
    // cleanly instead of freezing.
    function onConfirm() {
      try {
        // (yellow J7) No decoded image YET (the async decode hasn't landed) —
        // a plain no-op return would hang the modal; settling with null
        // cancels cleanly + keeps the prior portrait. A previous modal's
        // image can never be drawn (per-modal state).
        if (!loadedImg) { done(null); return; }
        const natural = loadedImg.naturalWidth || loadedImg.width;
        const naturalH = loadedImg.naturalHeight || loadedImg.height;
        if (!natural || !naturalH) { done(null); return; }
        // Map stage-px crop rect → natural-px source rect. The stage paints the
        // image via object-fit: cover, so the painted region is the largest
        // centered 2:3-ish crop of the source that fits the stage. Compute the
        // cover scale + offset to invert the mapping.
        const { w: sw, h: sh } = stageSize();
        const scale = Math.max(sw / natural, sh / naturalH);
        const paintedW = natural * scale;
        const paintedH = naturalH * scale;
        const offsetX = (paintedW - sw) / 2; // how much of the painted image is clipped left
        const offsetY = (paintedH - sh) / 2;
        // Source rect (natural px): the crop-rect's position on the painted
        // image, mapped back through the cover scale.
        const srcX = (rX + offsetX) / scale;
        const srcY = (rY + offsetY) / scale;
        const srcW = rW / scale;
        const srcH = rH / scale;
        // Clamp the source rect to the image (cover can clip, so this is a guard).
        const sx = Math.max(0, Math.min(srcX, natural - 1));
        const sy = Math.max(0, Math.min(srcY, naturalH - 1));
        const sw2 = Math.max(1, Math.min(srcW, natural - sx));
        const sh2 = Math.max(1, Math.min(srcH, naturalH - sy));
        // Draw to the output canvas at OUTPUT_W × OUTPUT_H (2:3).
        const canvas = document.createElement('canvas');
        canvas.width = OUTPUT_W;
        canvas.height = OUTPUT_H;
        const ctx = canvas.getContext('2d');
        ctx.imageSmoothingQuality = 'high';
        ctx.drawImage(loadedImg, sx, sy, sw2, sh2, 0, 0, OUTPUT_W, OUTPUT_H);
        const dataUrl = canvas.toDataURL('image/png');
        // bytes for the caller — shipped over IPC as base64-over-JSON
        // (bytesB64), never as a bare byte-array arg (anti-pattern #5).
        canvas.toBlob((blob) => {
          if (!blob) { done(null); return; }
          blob.arrayBuffer().then((buf) => {
            done({ dataUrl, bytes: new Uint8Array(buf), ext: 'png' });
          }, () => done(null));
        }, 'image/png');
      } catch (err) {
        // Never hang: surface + cancel so the slot keeps its prior state.
        console.error('[portrait-cropper] confirm failed:', err);
        done(null);
      }
    }

    confirmBtn.addEventListener('click', onConfirm);
    cancelBtn.addEventListener('click', () => done(null));
    // Esc cancels (top priority — no confirm-on-Esc).
    function onKey(e) {
      if (!overlay.isConnected) return;
      if (e.key === 'Escape') {
        e.stopPropagation();
        e.preventDefault();
        done(null);
      } else if (e.key === 'Enter') {
        e.stopPropagation();
        e.preventDefault();
        onConfirm();
      }
    }
    onDocKey = onKey;
    document.addEventListener('keydown', onKey, { capture: true });
    // Click on the backdrop (outside the modal) cancels.
    overlay.addEventListener('pointerdown', (e) => {
      if (e.target === overlay) done(null);
    });
    // Focus the confirm button so Enter works immediately.
    requestAnimationFrame(() => confirmBtn.focus());
  });
}
