// Disable a repository: the one confirmation and the one unbind call behind
// both Disable buttons, the org page's row and the repository's Settings tab.
//
// Disabling unbinds the repository from its connection, which removes its
// project with the project's environments, secrets and access grants. The
// repository on the forge is untouched, and nothing the forge sends enables
// it again. You type `owner/project` to confirm.
//
// The caller decides who sees the button: the row's `can_change`, which the
// server computes with the same rule the unbind endpoint applies.
import { unbindRepo, type OrgRepo } from "../api/client";
import ConfirmDestructive from "./ConfirmDestructive";

export default function DisableRepository(props: {
  org: string;
  /** The repository, as the org's repository list gives it. */
  repo: OrgRepo;
  /** After the server has unbound it. */
  onDisabled: () => void;
  onClose: () => void;
}) {
  const disable = async () => {
    await unbindRepo(props.repo.connection_id, props.repo.owner, props.repo.name);
    props.onDisabled();
  };
  return (
    <ConfirmDestructive
      eyebrow={`${props.org} · ${props.repo.connection_id}`}
      title={`Disable ${props.repo.owner}/${props.repo.name}?`}
      purpose="Its project is removed, with its environments, secrets and access grants."
      name={`${props.repo.owner}/${props.repo.project ?? props.repo.name}`}
      confirmLabel="Disable repository"
      onConfirm={disable}
      onClose={props.onClose}
    >
      <p>
        Pushes to it stop starting runs. The repository on the forge is untouched, and no
        installation event or re-sync enables it again. Enabling it later creates a new project
        without the old one's settings.
      </p>
    </ConfirmDestructive>
  );
}
