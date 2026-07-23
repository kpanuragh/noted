import "./globals.css";
import { AppShell } from "@/components/AppShell";

export const metadata = {
  title: "noted",
  description: "Your notes, and the knowledge graph built from them.",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>
        <AppShell>{children}</AppShell>
      </body>
    </html>
  );
}
