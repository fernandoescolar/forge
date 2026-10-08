// The package's version, from its package.json (one directory up from dist/).
import { readFileSync } from "node:fs";

export const VERSION: string = (() => {
  try {
    return JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")).version;
  } catch {
    return "0.0.0";
  }
})();
