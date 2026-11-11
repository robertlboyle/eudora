// Eudora's own questions and notices, in place of the browser's confirm / alert / prompt. Used
// by the main window and message windows.

/**
 * Asks a question. `buttons`: [{ label, value, primary? }], the first primary one answers Enter;
 * Esc or the × answers `null`. Returns the chosen button's value. `content`: an element shown above the buttons.
 */
function ask({ title, text, buttons, detail, content }) {
  return new Promise((resolve) => {
    const dialog = document.createElement("dialog");
    dialog.className = "ask";
    const h = document.createElement("h2");
    h.textContent = title;
    dialog.append(h);
    for (const para of String(text || "").split("\n\n")) {
      const p = document.createElement("p");
      p.textContent = para;
      dialog.append(p);
    }
    if (detail) {
      const p = document.createElement("p");
      p.className = "ask-detail";
      p.textContent = detail;
      dialog.append(p);
    }
    if (content) dialog.append(content); // (an element of the caller's: ticks, a list…)
    const row = document.createElement("div");
    row.className = "p-buttons";
    let answer = null;
    for (const b of buttons) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = b.label;
      if (b.primary) button.className = "primary";
      button.addEventListener("click", () => { answer = b.value; dialog.close(); });
      row.append(button);
    }
    dialog.append(row);
    const x = document.createElement("button");
    x.className = "dlg-x";
    x.type = "button";
    x.title = "Close";
    x.textContent = "✕";
    x.addEventListener("click", () => dialog.close());
    dialog.prepend(x);
    dialog.addEventListener("close", () => { dialog.remove(); resolve(answer); });
    document.body.append(dialog);
    dialog.showModal();
    (row.querySelector(".primary") || row.querySelector("button"))?.focus();
  });
}

/** A yes / no question; the buttons say what each does. Returns true for `yes`. */
async function askYesNo(title, text, yes, no = "Cancel") {
  return (await ask({ title, text, buttons: [{ label: no, value: false }, { label: yes, value: true, primary: true }] })) === true;
}

/** A notice with one button (the same notice already showing isn't shown twice). */
function tell(title, text, ok = "OK") {
  const same = [...document.querySelectorAll("dialog.ask[open] h2")].some((h) => h.textContent === title);
  if (same) return Promise.resolve(true);
  return ask({ title, text, buttons: [{ label: ok, value: true, primary: true }] });
}

/** Asks for a line of text (null when canceled). */
function askText(title, label, value = "", ok = "OK") {
  return new Promise((resolve) => {
    const dialog = document.createElement("dialog");
    dialog.className = "ask";
    const h = document.createElement("h2");
    h.textContent = title;
    const l = document.createElement("label");
    l.className = "ask-field";
    l.textContent = label;
    const input = document.createElement("input");
    input.type = "text";
    input.value = value;
    input.spellcheck = false;
    l.append(input);
    const row = document.createElement("div");
    row.className = "p-buttons";
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.textContent = "Cancel";
    const okButton = document.createElement("button");
    okButton.type = "button";
    okButton.className = "primary";
    okButton.textContent = ok;
    row.append(cancel, okButton);
    let answer = null;
    const done = () => { answer = input.value; dialog.close(); };
    okButton.addEventListener("click", done);
    cancel.addEventListener("click", () => dialog.close());
    input.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); done(); } });
    dialog.append(h, l, row);
    dialog.addEventListener("close", () => { dialog.remove(); resolve(answer); });
    document.body.append(dialog);
    dialog.showModal();
    input.focus();
    input.select();
  });
}
