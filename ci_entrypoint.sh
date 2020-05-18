#!/bin/bash -xe
#
# Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Entry point script for the CI running on an EC2 instance.
# It is responsible for:
#    1. Publishing status updates to github.
#    2. Running the tests.
#    3. Publishing the test logs in an S3 bucket.

# Set pipe fail option to capture return code after using pipe commands (e.g. tee)
set -o pipefail
SCRIPTDIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
cd "${SCRIPTDIR}"

# Update Github status, see:
#   https://developer.github.com/v3/repos/statuses/
function status_update() {
	curl -H "Authorization: token ${ACCESS_TOKEN}" \
		-X POST -d '{"state":"'"${STATE}"'","target_url": "'"${LOGS_URL}"'","description": "Test runner updated","context": "test_runner/ec2-instance"}'\
		https://api.github.com/repos/aws/aws-nitro-enclaves-cli/statuses/${CODEBUILD_RESOLVED_SOURCE_VERSION}
}

pwd
source build_env.txt

PR_NUMBER=$(echo "$CODEBUILD_SOURCE_VERSION" | cut -d"/" -f2)
LOGS_PATH="tests_results/${PR_NUMBER}/ci_logs_${CODEBUILD_RESOLVED_SOURCE_VERSION}.txt"
LOGS_URL="https://console.aws.amazon.com/s3/object/aws-nitro-enclaves-cli/${LOGS_PATH}"
ACCESS_TOKEN=$(aws ssm get-parameter --name GITHUB_TOKEN --region us-east-1 | jq -r .Parameter.Value)
if [[ $ACCESS_TOKEN == "" ]];
then
        echo "Invalid ACCESS_TOKEN"
        exit 1
fi

STATE="pending"
status_update

set +e
./run_tests.sh 2>&1 | tee test_logs.out
TEST_RESULTS=$?
set -e

aws s3 cp test_logs.out s3://aws-nitro-enclaves-cli/${LOGS_PATH}

STATE="success"
if [[ "${TEST_RESULTS}" != "0" ]];then
	STATE="failure"
fi

status_update

