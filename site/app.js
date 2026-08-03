const copyButton = document.querySelector("[data-copy-command]");
const copyLabel = document.querySelector("[data-copy-label]");
const toast = document.querySelector("[data-toast]");

async function copyText(text) {
  if (navigator.clipboard && window.isSecureContext) {
    await navigator.clipboard.writeText(text);
    return;
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);
  textarea.select();
  const copied = document.execCommand("copy");
  textarea.remove();
  if (!copied) throw new Error("copy command was rejected");
}

copyButton?.addEventListener("click", async () => {
  const command = copyButton.querySelector("code")?.textContent?.trim();
  if (!command) return;

  try {
    await copyText(command);
    copyLabel.textContent = "COPIED";
    toast.classList.add("visible");
    window.setTimeout(() => {
      copyLabel.textContent = "COPY";
      toast.classList.remove("visible");
    }, 2200);
  } catch {
    copyLabel.textContent = "SELECT";
    const selection = window.getSelection();
    const range = document.createRange();
    range.selectNodeContents(copyButton.querySelector("code"));
    selection.removeAllRanges();
    selection.addRange(range);
  }
});
