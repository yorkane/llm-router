"use client";

import HarmonyDemo from "@/components/HarmonyDemo";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const queryClient = new QueryClient();

export default function Home() {
  return (
    <QueryClientProvider client={queryClient}>
      <HarmonyDemo />
    </QueryClientProvider>
  );
}
