// =============================================================
// BACKGROUND CROPPER — a centered modal that lets the user crop a picked
// background image freely (4 corner handles, moveable frame) before it lands
// in the Fable Background Library.
//
// Sibling of portrait-cropper.js — SAME UX feel (the `.fable-crop-*` CSS, the
// 4 corner handles, moveable + resizable frame, largest-fit default, Confirm/
// Cancel, Esc cancels, Enter confirms), but DIFFERENT geometry:
//   - CONTAIN mode: the stage shows the WHOLE image (not cover-cropped), so the
//     user can crop any region of a non-16:9 source.
//   - FREE aspect: the 4 corners resize freely — NO ratio lock (the portrait
//     cropper locks 2:3). "as much as you want."
//   - Default frame = the largest centered 16:9 rect in the image (the sensible
//     default; a genuine 16:9 photo is a zero-adjust confirm). The user can then
//     break 16:9 freely.
//   - Output at the crop's NATURAL pixel resolution (full fidelity — backgrounds
//     are full-screen, unlike the portrait's fixed 480×720), format-preserving
//     (PNG source → PNG, JPEG → JPEG) via the canvas.
//
// Why a sibling, not a generalize of portrait-cropper: the portrait cropper is
// load-bearing (5 call sites) + its cover-mode/locked-aspect math genuinely
// differs from contain-mode/free-aspect. A separate module = zero blast radius
// to portrait picking; the shared visual identity comes from reusing the CSS
// classes. The stage box is resized inline here (overriding the portrait CSS's
// fixed 2/3) to match each image's own aspect.
//
// FLOW: openBackgroundCropper(parentEl, imageSrc) → loads the image, sizes the
// stage to its aspect, overlays the default 16:9 frame → on Confirm draws the
// cropped region to an offscreen canvas → resolves { dataUrl, bytes, ext }.
// On Cancel / Esc / backdrop / load-error → resolves null.
//
// `imageSrc` MUST be a same-origin `data:` URL (produced by
// fable_player_portrait_read_bytes) — a cross-origin `asset://` URL would
// taint the canvas + SecurityError on toBlob/toDataURL. The onConfirm try/catch
// is the belt-and-suspenders guard (mirrors portrait-cropper).
// =============================================================

const TARGET_ASPECT = 16 / 9; // the default frame's aspect (width / height)
const MIN_CROP = 48;          // stage-px floor on either edge — frame can't vanish
const MAX_LONG_EDGE = 3840;   // cap output so a huge crop doesn't bloat the library

// Cache the loaded image between the render + the confirm draw. One in-flight
// modal at a time (the gallery is modal).
let _loadedImg = null;

/**
 * Open the background cropper against `parentEl` with `imageSrc` (a same-origin
 * `data:` URL the browser can paint). Resolves { dataUrl, bytes, ext } on
 * Confirm, or null on Cancel / load error.
 */
export function openBackgroundCropper(parentEl, imageSrc) {
  return new Promise((resolve) => {
    // Build the modal DOM. Reuses the portrait cropper's CSS classes for visual
    // identity (flow-cinematic.css); only the stage box is resized inline below.
    const overlay = document.createElement('div');
    overlay.className = 'fable-crop-overlay';
    overlay.innerHTML = `
      <div class="fable-crop-modal" role="dialog" aria-modal="true" aria-label="Crop background">
        <h2 class="fable-crop-title">Crop Background</h2>
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
    function done(result) {
      if (settled) return;
      settled = true;
      overlay.classList.remove('is-open');
      // Wait for the fade-out transition before removing + resolving so the
      // modal doesn't snap out of existence (mirrors portrait-cropper).
      setTimeout(() => {
        overlay.remove();
        resolve(result);
      }, 200);
    }

    // Load the image. On error, cancel silently.
    const probe = new Image();
    probe.onload = () => {
      _loadedImg = probe;
      sizeStageToImage(probe.naturalWidth || probe.width, probe.naturalHeight || probe.height);
      img.src = imageSrc;
      // Defer one frame so layout settles, then place the default 16:9 frame.
      requestAnimationFrame(() => centerDefaultRect());
    };
    probe.onerror = () => done(null);
    probe.src = imageSrc;

    // --- Stage sizing (contain mode) -------------------------------------
    // Size the stage to the image's OWN aspect, fit within a landscape-friendly
    // viewport. In contain mode the stage IS the painted image (1:1), so the
    // crop rect maps directly to image pixels — no cover-inversion math. The
    // inline width/height/aspect-ratio override the portrait CSS's fixed 2/3.
    function sizeStageToImage(natural, naturalH) {
      if (!natural || !naturalH) return;
      const imageAspect = natural / naturalH;
      const maxW = Math.max(280, Math.min(window.innerWidth - 80, 1100));
      const maxH = Math.max(200, Math.min(window.innerHeight - 260, 680));
      let sw, sh;
      if (imageAspect >= maxW / maxH) {
        // image wider than the viewport ratio → width-bound
        sw = maxW;
        sh = Math.round(maxW / imageAspect);
      } else {
        sh = maxH;
        sw = Math.round(maxH * imageAspect);
      }
      stage.style.width = `${sw}px`;
      stage.style.height = `${sh}px`;
      stage.style.aspectRatio = 'auto';
      // Force exact fill (stage aspect == image aspect, so no distortion); avoids
      // any sub-pixel cover-clipping from the portrait CSS's object-fit: cover.
      img.style.objectFit = 'fill';
    }

    // --- Crop-rect geometry (stage-pixel space) --------------------------
    let rX = 0, rY = 0, rW = 0, rH = 0;

    function applyRect() {
      rect.style.left = `${rX}px`;
      rect.style.top = `${rY}px`;
      rect.style.width = `${rW}px`;
      rect.style.height = `${rH}px`;
      // Carve the rect out of the mask via an even-odd clip-path polygon (the
      // mask covers everything EXCEPT the rect, so the image shows through).
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

    // Default frame = largest centered 16:9 rect in the image. A genuine 16:9
    // photo = zero-adjust confirm; off-aspect images get the largest 16:9 region
    // the user can then shrink or reshape freely.
    function centerDefaultRect() {
      const { w, h } = stageSize();
      if (!w || !h) return;
      let fw, fh;
      if (w / h >= TARGET_ASPECT) {
        // stage wider than 16:9 → height-bound
        fh = h;
        fw = Math.round(fh * TARGET_ASPECT);
      } else {
        fw = w;
        fh = Math.round(fw / TARGET_ASPECT);
      }
      rW = Math.max(MIN_CROP, fw);
      rH = Math.max(MIN_CROP, fh);
      rX = Math.floor((w - rW) / 2);
      rY = Math.floor((h - rH) / 2);
      applyRect();
    }

    // --- Pointer interaction (drag to move, corners to resize — FREE) ----
    let drag = null; // { mode: 'move'|'nw'|'ne'|'sw'|'se', startX, startY, orig }

    function onPointerDown(e) {
      const handle = e.target.closest('[data-h]');
      const mode = handle ? handle.dataset.h : 'move';
      drag = { mode, startX: e.clientX, startY: e.clientY, orig: { x: rX, y: rY, w: rW, h: rH } };
      try { e.currentTarget.setPointerCapture(e.pointerId); } catch (_) {}
      e.preventDefault();
    }
    function onPointerMove(e) {
      if (!drag) return;
      const o = drag.orig;
      if (drag.mode === 'move') {
        const { w, h } = stageSize();
        const dx = e.clientX - drag.startX;
        const dy = e.clientY - drag.startY;
        rX = Math.max(0, Math.min(o.x + dx, w - o.w));
        rY = Math.max(0, Math.min(o.y + dy, h - o.h));
      } else {
        // Resize: the dragged corner follows the pointer (in stage coords); the
        // OPPOSITE corner stays fixed as the anchor. New rect = bounding box of
        // (anchor, clamped pointer). FREE aspect — no ratio re-derivation. Clamp
        // to the stage + enforce MIN_CROP so the frame can't invert or vanish.
        const stageRect = stage.getBoundingClientRect();
        const px = e.clientX - stageRect.left;
        const py = e.clientY - stageRect.top;
        const { w: sw, h: sh } = stageSize();
        const cpx = Math.max(0, Math.min(px, sw));
        const cpy = Math.max(0, Math.min(py, sh));
        // Anchor corner = the opposite of the dragged handle (original coords).
        let ax, ay;
        if (drag.mode === 'se') { ax = o.x; ay = o.y; }
        else if (drag.mode === 'nw') { ax = o.x + o.w; ay = o.y + o.h; }
        else if (drag.mode === 'ne') { ax = o.x; ay = o.y + o.h; }
        else /* sw */ { ax = o.x + o.w; ay = o.y; }
        let left = Math.min(ax, cpx);
        let top = Math.min(ay, cpy);
        let right = Math.max(ax, cpx);
        let bottom = Math.max(ay, cpy);
        // Enforce min size by expanding the dragged edge away from the anchor.
        if (right - left < MIN_CROP) {
          if (cpx >= ax) right = ax + MIN_CROP; else left = ax - MIN_CROP;
        }
        if (bottom - top < MIN_CROP) {
          if (cpy >= ay) bottom = ay + MIN_CROP; else top = ay - MIN_CROP;
        }
        rX = left; rY = top; rW = right - left; rH = bottom - top;
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
    // Wrapped in try/catch (mirrors portrait-cropper): a tainted-canvas
    // SecurityError must surface + cancel, never hang the modal.
    function onConfirm() {
      try {
        if (!_loadedImg) { done(null); return; }
        const natural = _loadedImg.naturalWidth || _loadedImg.width;
        const naturalH = _loadedImg.naturalHeight || _loadedImg.height;
        if (!natural || !naturalH) { done(null); return; }
        const { w: sw, h: sh } = stageSize();
        if (!sw || !sh) { done(null); return; }
        // Contain mode: the stage IS the image, so scale = natural / stage for
        // each axis (≈ equal since stage matches image aspect; separate axes is
        // robust against sub-pixel drift). Direct mapping — no cover inversion.
        const scaleX = natural / sw;
        const scaleY = naturalH / sh;
        let sx = rX * scaleX;
        let sy = rY * scaleY;
        let srcW = rW * scaleX;
        let srcH = rH * scaleY;
        // Clamp the source rect to the image (guard).
        sx = Math.max(0, Math.min(sx, natural - 1));
        sy = Math.max(0, Math.min(sy, naturalH - 1));
        srcW = Math.max(1, Math.min(srcW, natural - sx));
        srcH = Math.max(1, Math.min(srcH, naturalH - sy));
        // Output canvas at the crop's NATURAL pixel dims (full fidelity), capped
        // on the long edge so a pathological crop (e.g. a 12MP region) doesn't
        // bloat the library. A 1440p 16:9 crop lands well under the cap untouched.
        const longSide = Math.max(srcW, srcH);
        const outScale = longSide > MAX_LONG_EDGE ? MAX_LONG_EDGE / longSide : 1;
        const outW = Math.max(1, Math.round(srcW * outScale));
        const outH = Math.max(1, Math.round(srcH * outScale));
        const canvas = document.createElement('canvas');
        canvas.width = outW;
        canvas.height = outH;
        const ctx = canvas.getContext('2d');
        ctx.imageSmoothingQuality = 'high';
        ctx.drawImage(_loadedImg, sx, sy, srcW, srcH, 0, 0, outW, outH);
        // Format-preserving: PNG source → PNG (lossless, good for graphics with
        // text), JPEG source → JPEG @ 0.95 (smaller, good for photos). Detect
        // from the input data URL prefix.
        const isPng = /^data:image\/png/i.test(imageSrc);
        const mime = isPng ? 'image/png' : 'image/jpeg';
        const ext = isPng ? 'png' : 'jpg';
        const quality = isPng ? undefined : 0.95;
        const dataUrl = canvas.toDataURL(mime, quality);
        canvas.toBlob((blob) => {
          if (!blob) { done(null); return; }
          blob.arrayBuffer().then((buf) => {
            done({ dataUrl, bytes: new Uint8Array(buf), ext });
          }, () => done(null));
        }, mime, quality);
      } catch (err) {
        console.error('[background-cropper] confirm failed:', err);
        done(null);
      }
    }

    confirmBtn.addEventListener('click', onConfirm);
    cancelBtn.addEventListener('click', () => done(null));
    function onKey(e) {
      if (e.key === 'Escape' && !settled) {
        e.stopPropagation();
        e.preventDefault();
        done(null);
      } else if (e.key === 'Enter' && !settled) {
        e.stopPropagation();
        e.preventDefault();
        onConfirm();
      }
    }
    overlay.addEventListener('keydown', onKey);
    // Click on the backdrop (outside the modal) cancels.
    overlay.addEventListener('pointerdown', (e) => {
      if (e.target === overlay) done(null);
    });
    requestAnimationFrame(() => confirmBtn.focus());
  });
}
