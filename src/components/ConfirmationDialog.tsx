import { useEffect, useRef } from "react";

type ConfirmationDialogProps = {
  title: string;
  message: string;
  confirmLabel: string;
  busy?: boolean;
  busyLabel?: string;
  variant?: "danger" | "primary";
  onCancel: () => void;
  onConfirm: () => void;
};

function ConfirmationDialog({
  title,
  message,
  confirmLabel,
  busy = false,
  busyLabel = "Working…",
  variant = "danger",
  onCancel,
  onConfirm,
}: ConfirmationDialogProps) {
  const cancelButtonRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    cancelButtonRef.current?.focus();

    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape" && !busy) {
        onCancel();
      }
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [busy, onCancel]);

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={busy ? undefined : onCancel}>
      <section
        className="confirmation-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="confirmation-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="dialog-icon" aria-hidden="true">
          !
        </div>
        <div className="dialog-copy">
          <h2 id="confirmation-title">{title}</h2>
          <p>{message}</p>
        </div>
        <div className="dialog-actions">
          <button
            ref={cancelButtonRef}
            className="secondary-button"
            type="button"
            onClick={onCancel}
            disabled={busy}
          >
            Cancel
          </button>
          <button className={variant === "primary" ? "primary-button" : "destructive-button"} type="button" onClick={onConfirm} disabled={busy}>
            {busy ? busyLabel : confirmLabel}
          </button>
        </div>
      </section>
    </div>
  );
}

export default ConfirmationDialog;
