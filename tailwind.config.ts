import type { Config } from "tailwindcss";

export default {
  content: ["./index.html", "./desktop/**/*.{ts,tsx}"],
  theme: {
    extend: {
      fontFamily: {
        sans: ["Segoe WPC", "Segoe UI", "system-ui", "-apple-system", "BlinkMacSystemFont", "sans-serif"],
        mono: ["Cascadia Mono", "Cascadia Code", "Consolas", "Courier New", "monospace"],
      },
    },
  },
  plugins: [],
} satisfies Config;
