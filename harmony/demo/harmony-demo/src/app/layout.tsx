import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "Harmony Demo",
  description: "Harmony Demo",
  icons: {
    icon: "/openai_logo.svg",
  },
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body className={`antialiased`}>{children}</body>
    </html>
  );
}
