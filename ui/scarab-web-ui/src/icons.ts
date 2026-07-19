// Lucide icon data (the framework-agnostic `lucide` package exports each icon as
// an IconNode — an array of [tag, attrs] primitives on a 24×24 canvas). We use
// the SAME data two ways: rendered crisp for functional UI (see components/Icon)
// and re-drawn dotted for the background doodles
// (components/Doodle) — per the doodle system in docs/DESIGN.md §5.
import {
  Bug,
  GitBranch,
  GitCommitHorizontal,
  GitPullRequest,
  Container,
  Boxes,
  Workflow,
  Waypoints,
  Package,
  Terminal,
  KeyRound,
  ShieldCheck,
  Timer,
  Play,
  CircleDot,
  Plus,
  Search,
  ChevronRight,
  ChevronDown,
  Tag,
  Settings,
  Rocket,
  ArrowUp,
  ArrowLeft,
  RotateCw,
  RotateCcw,
  History,
  AlertTriangle,
  File,
  Folder,
  Sun,
  Moon,
} from "lucide";

// IconNode = Array<[tagName, attributes]>.
export type IconNode = ReadonlyArray<readonly [string, Record<string, string | number>]>;

const ICONS: Record<string, IconNode> = {
  bug: Bug as IconNode,
  "git-branch": GitBranch as IconNode,
  "git-commit-horizontal": GitCommitHorizontal as IconNode,
  container: Container as IconNode,
  boxes: Boxes as IconNode,
  workflow: Workflow as IconNode,
  waypoints: Waypoints as IconNode,
  package: Package as IconNode,
  terminal: Terminal as IconNode,
  "key-round": KeyRound as IconNode,
  "shield-check": ShieldCheck as IconNode,
  timer: Timer as IconNode,
  play: Play as IconNode,
  "circle-dot": CircleDot as IconNode,
  "git-pull-request": GitPullRequest as IconNode,
  plus: Plus as IconNode,
  search: Search as IconNode,
  "chevron-right": ChevronRight as IconNode,
  "chevron-down": ChevronDown as IconNode,
  tag: Tag as IconNode,
  settings: Settings as IconNode,
  rocket: Rocket as IconNode,
  "arrow-up": ArrowUp as IconNode,
  "arrow-left": ArrowLeft as IconNode,
  "rotate-cw": RotateCw as IconNode,
  "rotate-ccw": RotateCcw as IconNode,
  history: History as IconNode,
  "alert-triangle": AlertTriangle as IconNode,
  file: File as IconNode,
  folder: Folder as IconNode,
  sun: Sun as IconNode,
  moon: Moon as IconNode,
};

export function iconNode(name: string): IconNode | undefined {
  return ICONS[name];
}
