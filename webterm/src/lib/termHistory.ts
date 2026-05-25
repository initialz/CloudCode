const DB_NAME = 'cloudcode-webterm';
const STORE_NAME = 'term-history';
const DB_VERSION = 1;

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, DB_VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE_NAME)) {
        db.createObjectStore(STORE_NAME);
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

export async function saveTermState(key: string, data: string): Promise<void> {
  try {
    const db = await open();
    const tx = db.transaction(STORE_NAME, 'readwrite');
    tx.objectStore(STORE_NAME).put(data, key);
    db.close();
  } catch {
    // IndexedDB unavailable (private browsing, etc.) — silently skip
  }
}

export async function loadTermState(key: string): Promise<string | null> {
  try {
    const db = await open();
    return new Promise((resolve) => {
      const tx = db.transaction(STORE_NAME, 'readonly');
      const req = tx.objectStore(STORE_NAME).get(key);
      req.onsuccess = () => resolve(req.result ?? null);
      req.onerror = () => resolve(null);
      db.close();
    });
  } catch {
    return null;
  }
}

export async function deleteTermState(key: string): Promise<void> {
  try {
    const db = await open();
    const tx = db.transaction(STORE_NAME, 'readwrite');
    tx.objectStore(STORE_NAME).delete(key);
    db.close();
  } catch {
    // ignore
  }
}
