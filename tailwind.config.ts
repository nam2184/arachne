import type { Config } from "tailwindcss";

export default {
  content: ["./index.html", "./desktop/**/*.{ts,tsx}"],
  theme: {
    extend: {},
  },
  plugins: [],
} satisfies Config;
