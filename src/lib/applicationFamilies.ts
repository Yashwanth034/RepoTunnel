import type { LaunchApplication } from "../types";

export type ApplicationFamily = {
  id: string;
  name: string;
  description: string;
  members: Array<{
    id: string;
    label: string;
  }>;
};

export const PRODUCTIVITY_APPLICATION_FAMILIES: ApplicationFamily[] = [
  {
    id: "microsoft-365",
    name: "Microsoft 365",
    description: "Word · Excel · PowerPoint",
    members: [
      { id: "microsoft-word", label: "Word" },
      { id: "microsoft-excel", label: "Excel" },
      { id: "microsoft-powerpoint", label: "PowerPoint" },
    ],
  },
  {
    id: "libreoffice",
    name: "LibreOffice",
    description: "Writer · Calc · Impress",
    members: [
      { id: "libreoffice-writer", label: "Writer" },
      { id: "libreoffice-calc", label: "Calc" },
      { id: "libreoffice-impress", label: "Impress" },
    ],
  },
  {
    id: "apple-iwork",
    name: "Apple iWork",
    description: "Pages · Numbers · Keynote",
    members: [
      { id: "apple-pages", label: "Pages" },
      { id: "apple-numbers", label: "Numbers" },
      { id: "apple-keynote", label: "Keynote" },
    ],
  },
];

const productivityIds = new Set(
  PRODUCTIVITY_APPLICATION_FAMILIES.flatMap((family) => family.members.map((member) => member.id)),
);

export function isProductivityApplication(application: LaunchApplication): boolean {
  return productivityIds.has(application.id);
}

export function detectedProductivityFamilies(applications: LaunchApplication[]) {
  const byId = new Map(applications.map((application) => [application.id, application]));
  return PRODUCTIVITY_APPLICATION_FAMILIES.map((family) => ({
    ...family,
    detectedMembers: family.members.flatMap((member) => {
      const application = byId.get(member.id);
      return application ? [{ ...member, application }] : [];
    }),
  })).filter((family) => family.detectedMembers.length > 0);
}
