;;; emskin-child-frame.el --- Keep child frames above embedded apps  -*- lexical-binding: t; -*-

;; pgtk paints child frames (corfu, posframe, company-posframe, …) into
;; the parent frame's own Wayland surface, so an embedded app mapped
;; above Emacs hides them.  Report every visible child frame's rect to
;; the compositor, which re-draws those regions of Emacs above the apps
;; and routes pointer input there back to Emacs.

;;; Code:

(require 'emskin-ipc)
(require 'emskin-app)  ; emskin--frame-header-offset

(defvar emskin--process)  ; defined in `emskin.el'

(defvar emskin--child-frames-last-sent nil
  "Last rect list sent with `set_child_frames', to suppress duplicates.")

(defun emskin--child-frame-visible-p (frame)
  "Non-nil if child FRAME and all of its child-frame ancestors are visible."
  (and (eq (frame-visible-p frame) t)
       (let ((parent (frame-parent frame)))
         (or (null (frame-parent parent))
             (emskin--child-frame-visible-p parent)))))

(defun emskin--child-frame-origin (frame)
  "Return (X . Y) of child FRAME's top-left within its root frame's native area.
A child frame's position is relative to its parent's native origin, and a
child frame has no decorations, so offsets accumulate up the ancestor chain."
  (let ((x 0) (y 0))
    (while (frame-parent frame)
      (let ((pos (frame-position frame)))
        (setq x (+ x (car pos))
              y (+ y (cdr pos))
              frame (frame-parent frame))))
    (cons x y)))

(defun emskin--child-frame-rects ()
  "Return rects (x y w h) of the selected root frame's visible child frames.
Coordinates are relative to the root frame's Wayland surface."
  (let ((root (frame-root-frame (selected-frame)))
        rects)
    (dolist (frame (frame-list))
      (when (and (frame-parent frame)
                 (eq (frame-root-frame frame) root)
                 (emskin--child-frame-visible-p frame))
        (let ((origin (emskin--child-frame-origin frame)))
          (push (list (car origin)
                      (+ (cdr origin) (emskin--frame-header-offset root))
                      (frame-native-width frame)
                      (frame-native-height frame))
                rects))))
    (nreverse rects)))

(defun emskin--child-frames-sync (&rest _)
  "Push the current child-frame rects if they changed since the last push."
  (when emskin--process
    (let ((rects (emskin--child-frame-rects)))
      (unless (equal rects emskin--child-frames-last-sent)
        (setq emskin--child-frames-last-sent rects)
        (emskin--send
         `((type . "set_child_frames")
           (rects . ,(vconcat
                      (mapcar (lambda (r)
                                `((x . ,(nth 0 r)) (y . ,(nth 1 r))
                                  (w . ,(nth 2 r)) (h . ,(nth 3 r))))
                              rects)))))))))

(defun emskin--child-frames-resync ()
  "Forget the last pushed rects and push the current ones."
  (setq emskin--child-frames-last-sent nil)
  (emskin--child-frames-sync))

(defun emskin--child-frames-sync-later (&rest _)
  "Sync after the current command, once a deleted frame is really gone."
  (run-at-time 0 nil #'emskin--child-frames-sync))

;; Child frames are shown, moved and hidden from timers as often as from
;; commands (corfu-auto, posframe), so hook redisplay rather than
;; `post-command-hook'.
(add-hook 'pre-redisplay-functions #'emskin--child-frames-sync)
(add-hook 'delete-frame-functions #'emskin--child-frames-sync-later)
(add-hook 'emskin-connected-hook #'emskin--child-frames-resync)

(provide 'emskin-child-frame)
;;; emskin-child-frame.el ends here
