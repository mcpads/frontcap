import { lib } from "https://api.example.test/lib.mjs";
postMessage(["imported", Date.now() - 946684800000, lib]);
setTimeout(() => postMessage(["t30", Date.now() - 946684800000, lib]), 30);
