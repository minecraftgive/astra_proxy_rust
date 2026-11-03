const DB = "astra-draft-recovery";
export interface Recovery {
  text: string;
  revision: string;
  draftId: string;
  original?: string;
  sourceRevision?: string;
  saveOperationId?: string;
}
const writes = new Map<string, Promise<void>>();
async function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open(DB, 1);
    r.onupgradeneeded = () => r.result.createObjectStore("drafts");
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
    r.onblocked = () =>
      reject(new Error("Browser recovery storage is blocked"));
  });
}
async function write(key: string, value: Recovery) {
  const db = await open();
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction("drafts", "readwrite");
      tx.objectStore("drafts").put(value, key);
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
      tx.onabort = () =>
        reject(tx.error ?? new Error("Browser recovery write aborted"));
    });
  } finally {
    db.close();
  }
}
export function retain(key: string, value: Recovery): Promise<void> {
  const next = (writes.get(key) ?? Promise.resolve())
    .catch(() => {})
    .then(() => write(key, value));
  writes.set(key, next);
  void next
    .finally(() => {
      if (writes.get(key) === next) writes.delete(key);
    })
    .catch(() => {});
  return next;
}
export async function recover(key: string): Promise<Recovery | undefined> {
  await writes.get(key)?.catch(() => {});
  const db = await open();
  try {
    return await new Promise((resolve, reject) => {
      const r = db.transaction("drafts").objectStore("drafts").get(key);
      r.onsuccess = () => resolve(r.result);
      r.onerror = () => reject(r.error);
    });
  } finally {
    db.close();
  }
}
