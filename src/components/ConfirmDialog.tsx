export function ConfirmDialog({
  title,
  description,
  item,
  danger,
  confirmText,
  onCancel,
  onConfirm,
}: {
  title: string;
  description: string;
  item: string;
  danger?: boolean;
  confirmText: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="confirm-backdrop" onMouseDown={onCancel}>
      <section
        className="confirm-dialog"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className={`confirm-icon${danger ? " danger" : ""}`}>{danger ? "!" : "−"}</div>
        <div>
          <p className="eyebrow">PLEASE CONFIRM</p>
          <h2 id="confirm-title">{title}</h2>
          <p>{description}</p>
          <strong title={item}>{item}</strong>
        </div>
        <div className="confirm-actions">
          <button className="ghost" autoFocus onClick={onCancel}>
            取消
          </button>
          <button className={danger ? "dialog-danger" : "primary"} onClick={onConfirm}>
            {confirmText}
          </button>
        </div>
      </section>
    </div>
  );
}
