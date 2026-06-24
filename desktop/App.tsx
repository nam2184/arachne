import { BrowserRouter, Routes, Route } from "react-router-dom";
import { AppShell } from "@/app/layout/AppShell";

export function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route path="/*" element={<AppShell />} />
      </Routes>
    </BrowserRouter>
  );
}