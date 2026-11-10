// Disable a repository: the one confirmation and the one unbind call behind
// every Disable button: the org page's row, the repository's Settings tab and
// a connection's project row under Organization settings › Connections.
//
// Disabling unbinds the repository from its connection, which removes its
// project with the project's environments, secrets and access grants. The
// repository on the forge is untouched, and nothing the forge sends enables
// it again. You type `owner/project` to confirm.
//
// The caller decides who sees the button, with the rule the unbind endpoint
// applies: Administer on the org, and a connection not managed by
// configuration.
import { unbindRepo } from "../api/client";
import ConfirmDestructive from "./ConfirmDestructive";

/** The repository to disable: the connection it is bound through and its
 *  coordinate on the forge. An org page row (`OrgRepo`) is one. */
export interface DisableTarget {
  connection_id: string;
  owner: string;
  name: string;
  /** Its project's name, when known; the name typed to confirm. */
  project?: string | null;
}

export default function DisableRepository(props: {
  org: string;
  repo: DisableTarget;
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
