// Print the path of the odiff binary that `odiff-bin` ships for this
// platform, for the Rust parity tests.
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { findBinary } = require("odiff-bin/binary");

process.stdout.write(findBinary());
