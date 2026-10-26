//! 每卷独立决定补读或重建，不因新加一个卷而重扫其它卷。
use std::collections::HashSet;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StartPlan {
    pub scan: Vec<char>,
    pub catch_up: Vec<char>,
}

pub fn plan_start(disks: &[char], marked: &HashSet<char>) -> StartPlan {
    let (catch_up, scan) = disks
        .iter()
        .copied()
        .partition(|disk| marked.contains(disk));
    StartPlan { scan, catch_up }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_unmarked_volumes_are_scanned() {
        assert_eq!(
            plan_start(&['C', 'D'], &HashSet::from(['C'])),
            StartPlan {
                scan: vec!['D'],
                catch_up: vec!['C'],
            }
        );
    }
    #[test]
    fn empty_and_fresh_install_plans() {
        assert_eq!(plan_start(&[], &HashSet::new()), StartPlan::default());
        assert_eq!(plan_start(&['C'], &HashSet::new()).scan, vec!['C']);
    }
}
