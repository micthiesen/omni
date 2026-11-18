// The phone's copy of the library: a thin promise wrapper over IndexedDB.
// Values are JSON strings written by the wasm side, except in `blobs`, which
// holds the bytes of photos waiting to upload.

const STORES = ['recipes', 'sessions', 'kv', 'outbox', 'blobs'];
let opening;

function open() {
  opening ??= new Promise((resolve, reject) => {
    const request = indexedDB.open('fond', 1);
    request.onupgradeneeded = () => {
      for (const name of STORES) {
        if (!request.result.objectStoreNames.contains(name)) request.result.createObjectStore(name);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
    request.onblocked = () => reject(new Error('IndexedDB is blocked'));
  });
  return opening;
}

async function run(stores, mode, work) {
  const db = await open();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(stores, mode);
    let result;
    tx.oncomplete = () => resolve(result);
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error || new Error('IndexedDB transaction aborted'));
    result = work(tx);
  });
}

// Every [key, value] of a store, in key order.
export async function idbAll(store) {
  const rows = [];
  await run([store], 'readonly', (tx) => {
    tx.objectStore(store).openCursor().onsuccess = (event) => {
      const cursor = event.target.result;
      if (cursor) {
        rows.push([cursor.key, cursor.value]);
        cursor.continue();
      }
    };
  });
  return rows;
}

export async function idbGet(store, key) {
  let request;
  await run([store], 'readonly', (tx) => {
    request = tx.objectStore(store).get(key);
  });
  return request.result;
}

// Applies writes to several stores in one transaction, so a record and the
// outbox entry that carries it to the server are stored together or not at
// all. Each op is [store, key, value] (put) or [store, key] (delete).
export function idbTx(ops) {
  const stores = [...new Set(ops.map((op) => op[0]))];
  return run(stores, 'readwrite', (tx) => {
    for (const [store, key, ...value] of ops) {
      if (value.length) tx.objectStore(store).put(value[0], key);
      else tx.objectStore(store).delete(key);
    }
    // Commit now instead of when the requests report back: a write must
    // not be lost to a reload or to the app being closed just after a tap.
    tx.commit?.();
  });
}

// Applies puts ([key, value] pairs) and deletes (keys) in one transaction;
// `clear` empties the store first.
export function idbWrite(store, puts, deletes, clear) {
  return run([store], 'readwrite', (tx) => {
    const target = tx.objectStore(store);
    if (clear) target.clear();
    for (const key of deletes) target.delete(key);
    for (const [key, value] of puts) target.put(value, key);
    // Commit now instead of when the requests report back: a write must
    // not be lost to a reload or to the app being closed just after a tap.
    tx.commit?.();
  });
}
