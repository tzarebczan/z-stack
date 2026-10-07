import { defineConfig } from "vite";
import { zStack } from "@z-stack/sdk/vite";

export default defineConfig({ plugins: [zStack()], build: { target: "es2022" } });
